//! Supply-chain cooldown (minimum release age) — `<cache_root>/cooldown_state.json`
//! の read/write と「この commit へ今進んでいいか」の pure 判定ロジック。
//!
//! 背景 (#supply-chain):
//! - npm / pnpm は 2025 年の Shai-Hulud 系攻撃を受けて `minimumReleaseAge`
//!   (publish から N 時間未満のバージョンを入れない) を導入した (pnpm 11 は
//!   既定 1d)。lazy.nvim にも同種の提案がある (folke/lazy.nvim#2141)。
//!   悪性 commit は大抵数時間〜数日で検出・撤去されるので、「新しすぎる
//!   ものを掴まない」だけで攻撃ウィンドウの大半を外せる。
//! - git には registry の publish 時刻に相当する信頼できる時刻が無い。
//!   committer date は攻撃者が自由に偽装 (backdate) できるため、それ単独を
//!   信頼すると mitigation ごと無効化される。
//! - そこで **「rvpm がその commit を最初に観測した時刻 (`first_seen`)」を
//!   主軸**にする。fetch のたびに remote tip を観測として記録し、cooldown
//!   を超えて観測され続けた commit だけを update の checkout 対象にする。
//!   first_seen はローカルで記録するので攻撃者には偽装できない。
//! - committer date (`committed_at`) は補助としてだけ使う: 「commit 自体が
//!   十分古い」ものは初観測でも即適用してよい (休眠 repo を初回 update で
//!   不要に held-back にしない)。backdate でこの枝を抜けられる点は既知の
//!   トレードオフ (docs/architecture.md に明記)。
//!
//! 適用範囲:
//! - `rvpm update` の「remote tip へ進む」判定だけをゲートする。明示 `rev`
//!   ピン / dev plugin / 初回インストール (clone) は対象外。
//! - `rvpm sync` はゲートしない (lockfile pin が既に新 commit を遮断する)
//!   が、fetch のついでに観測だけは記録して cooldown の熟成を進める。
//!
//! スキーマ:
//! ```json
//! { "version": 1,
//!   "entries": [
//!     { "name": "snacks.nvim", "url": "folke/snacks.nvim",
//!       "observed": [
//!         { "commit": "abc...", "first_seen": "2026-06-01T12:34:56Z",
//!           "committed_at": "2026-06-01T10:00:00Z" }
//!       ] }
//!   ] }
//! ```
//!
//! - **場所**: `<cache_root>/cooldown_state.json` (ephemeral cache 側。
//!   消えても安全側 = 全 tip が「初観測」に戻り held-back されるだけ)。
//! - **resilience**: malformed / missing → empty state にフォールバック。
//! - **schema version**: 未対応バージョンは empty 扱い (fetch_state と同じ)。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::update_log::{format_rfc3339_utc, parse_rfc3339_utc};

/// 現行スキーマバージョン。壊すときは bump + migration を足す。
pub const CURRENT_VERSION: u32 = 1;

/// 1 plugin あたりの観測履歴の上限。fetch ごとに最大 1 エントリしか増えず、
/// prune が eligible を 1 つに畳むので通常ここまで溜まらない (防波堤)。
pub const MAX_OBSERVED: usize = 200;

/// cooldown の既定値 (pnpm 11 と同じ 1d)。`options.cooldown` 未指定時の
/// **デフォルト ON** 値であり、設定文字列のパース失敗時の fail-closed
/// フォールバックも兼ねる (どちらも安全側に倒したいので同じ 1d)。
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);

/// cooldown_state のルート構造。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CooldownState {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<CooldownEntry>,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}

impl Default for CooldownState {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            entries: Vec::new(),
        }
    }
}

/// 1 プラグイン分の観測履歴。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CooldownEntry {
    /// `Plugin::display_name()` 由来の lookup キー。
    pub name: String,
    /// config.toml に書かれた URL。同じ name で別 repo に差し替えられた時の
    /// 検知用 (lockfile / fetch_state と同じ思想)。URL 不一致なら観測履歴は
    /// 信用せず捨てる (別リポジトリの commit 履歴だから)。
    pub url: String,
    /// remote tip として観測した commit の履歴 (新しいものが後ろとは限らない)。
    #[serde(default)]
    pub observed: Vec<ObservedCommit>,
}

/// 観測済み commit 1 件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedCommit {
    /// commit SHA (full)。
    pub commit: String,
    /// rvpm がこの commit を remote tip として最初に観測した時刻。
    /// RFC3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`)。ローカル記録なので偽装不能。
    pub first_seen: String,
    /// commit の committer date。偽装可能なので「十分古い commit は初観測でも
    /// 即適用」の補助判定にのみ使う。読めなかった場合は None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_at: Option<String>,
}

impl CooldownState {
    /// `path` から読み出す。
    /// - 存在しない → `Default` (empty)
    /// - パース失敗 / version mismatch → warn を出して `Default`
    pub fn load(path: &Path) -> Self {
        let content = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                eprintln!(
                    "\u{26a0} cooldown_state: failed to read {}: {} (treating as empty)",
                    path.display(),
                    e
                );
                return Self::default();
            }
        };
        match serde_json::from_str::<CooldownState>(&content) {
            Ok(s) if s.version == CURRENT_VERSION => s,
            Ok(s) => {
                eprintln!(
                    "\u{26a0} cooldown_state: unsupported version {} in {} (expected {}; treating as empty)",
                    s.version,
                    path.display(),
                    CURRENT_VERSION
                );
                Self::default()
            }
            Err(e) => {
                eprintln!(
                    "\u{26a0} cooldown_state: failed to parse {}: {} (treating as empty)",
                    path.display(),
                    e
                );
                Self::default()
            }
        }
    }

    /// `path` に atomic write する。書き出し前に `entries` を name で安定 sort
    /// して、同じ内容なら同じバイト列になるようにする。
    pub fn save(&mut self, path: &Path) -> Result<()> {
        self.entries.sort_by(|a, b| a.name.cmp(&b.name));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(self).context("serialize cooldown_state")?;
        let parent = path.parent().unwrap_or(Path::new("."));
        let tmp = tempfile::Builder::new()
            .prefix(".rvpm-cooldown-state-")
            .suffix(".tmp")
            .tempfile_in(parent)
            .with_context(|| format!("create tempfile in {}", parent.display()))?;
        std::fs::write(tmp.path(), body.as_bytes())
            .with_context(|| format!("write tempfile {}", tmp.path().display()))?;
        tmp.persist(path)
            .map_err(|e| anyhow::anyhow!("rename tempfile to {}: {}", path.display(), e))?;
        Ok(())
    }

    pub fn find(&self, name: &str) -> Option<&CooldownEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn upsert(&mut self, entry: CooldownEntry) {
        if let Some(slot) = self.entries.iter_mut().find(|e| e.name == entry.name) {
            *slot = entry;
        } else {
            self.entries.push(entry);
        }
    }

    /// `names` に無い entry を drop する (config.toml から外されたプラグイン)。
    pub fn retain_by_names(&mut self, names: &std::collections::HashSet<String>) {
        self.entries.retain(|e| names.contains(&e.name));
    }
}

/// update タスクへ渡す per-plugin の cooldown 入力 (caller が組み立てる)。
/// `cooldown` は実効値 (> ZERO のときだけ作られる)、`observed` は state から
/// 引いた当該 plugin の観測履歴スナップショット (URL 不一致時は空)。
#[derive(Debug, Clone)]
pub struct PluginCooldownCtx {
    pub cooldown: Duration,
    pub observed: Vec<ObservedCommit>,
}

/// update タスクが返す cooldown 結果。`observed` は観測追記 + prune 済みの
/// 最新履歴 (caller が state へ upsert して永続化する)。
#[derive(Debug, Clone)]
pub struct CooldownOutcome {
    pub observed: Vec<ObservedCommit>,
    pub held: Option<HeldByCooldown>,
}

/// cooldown により tip へ進まなかった plugin のサマリ表示用情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldByCooldown {
    /// 適用を見送った remote tip。
    pub tip: String,
    /// tip の初観測時刻 (RFC3339)。表示用 (「first seen 3h ago」)。
    pub tip_first_seen: Option<String>,
    /// 代わりに進んだ熟成済み commit (None = 現状維持)。
    pub fallback: Option<String>,
}

/// RFC3339 時刻から `now` までの経過を `"3h"` / `"2d"` / `"5m"` 風に丸める
/// (held-back サマリ表示用)。パース不能 / 未来時刻は `"?"`。
pub fn humanize_age(rfc3339: &str, now: SystemTime) -> String {
    let Some(then) = parse_rfc3339_utc(rfc3339) else {
        return "?".to_string();
    };
    let Ok(elapsed) = now.duration_since(then) else {
        return "?".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 60 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 24 * 60 * 60 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Duration を `"1d"` / `"12h"` 風の短い表記にする (サマリの「needs 1d」用)。
pub fn humanize_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return "0".to_string();
    }
    if secs % 86400 == 0 {
        format!("{}d", secs / 86400)
    } else if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

/// update が tip に対して取るべき行動。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CooldownDecision {
    /// tip は熟成済み (または cooldown 無効 / no-op) — そのまま進めてよい。
    Advance,
    /// tip は若すぎる。`fallback = Some(sha)` なら「観測済みで熟成済み、かつ
    /// 現 HEAD より新しく観測された commit」へ代わりに進む。`None` なら現状維持。
    Hold {
        /// 代わりに checkout してよい熟成済み commit (現 HEAD より前進する時のみ)。
        fallback: Option<String>,
    },
}

/// この観測 commit が「熟成済み (= cooldown を超えた)」か。
///
/// 判定 (OR):
/// - `first_seen` から `cooldown` 以上経過した (主軸; ローカル観測なので偽装不能)
/// - `committed_at` から `cooldown` 以上経過した (補助; 休眠 repo の古い commit
///   を初観測でも即適用するため。committer date は偽装可能な点は既知の妥協)
///
/// 時計逆行 (タイムスタンプが未来) やパース不能は「熟成していない」に倒す
/// (安全側 = 適用しない)。
pub fn is_eligible(obs: &ObservedCommit, now: SystemTime, cooldown: Duration) -> bool {
    let aged = |raw: &str| -> bool {
        parse_rfc3339_utc(raw)
            .and_then(|then| now.duration_since(then).ok())
            .is_some_and(|elapsed| elapsed >= cooldown)
    };
    if aged(&obs.first_seen) {
        return true;
    }
    obs.committed_at.as_deref().is_some_and(aged)
}

/// `commit` を観測履歴へ追記する。既に観測済みなら **first_seen を更新しない**
/// (更新すると永遠に熟成しない)。新規追加したら true。
pub fn observe(
    observed: &mut Vec<ObservedCommit>,
    commit: &str,
    committed_at: Option<SystemTime>,
    now: SystemTime,
) -> bool {
    if observed.iter().any(|o| o.commit == commit) {
        return false;
    }
    observed.push(ObservedCommit {
        commit: commit.to_string(),
        first_seen: format_rfc3339_utc(now),
        committed_at: committed_at.map(format_rfc3339_utc),
    });
    true
}

/// remote tip へ進んでよいか判定する pure function。
///
/// 決定表:
/// - `cooldown == ZERO` (無効) → Advance
/// - `tip == current_head` (no-op) → Advance
/// - tip が観測済みかつ熟成済み → Advance
/// - それ以外 (tip が若い / 未観測):
///   - 現 HEAD が観測履歴に無い → `Hold { fallback: None }` (現状維持。
///     fallback で**過去へ巻き戻す**事故を防ぐため、比較できない時は動かない)
///   - 熟成済みで「現 HEAD より後に観測された」commit があれば、その中で
///     最も新しく観測されたものを `Hold { fallback: Some(..) }` で返す
///     (アクティブな repo でも cooldown 分遅れで前進し続けるための要)
///   - 無ければ `Hold { fallback: None }`
pub fn decide(
    observed: &[ObservedCommit],
    tip: &str,
    current_head: Option<&str>,
    now: SystemTime,
    cooldown: Duration,
) -> CooldownDecision {
    if cooldown.is_zero() {
        return CooldownDecision::Advance;
    }
    if current_head == Some(tip) {
        return CooldownDecision::Advance;
    }
    if observed
        .iter()
        .find(|o| o.commit == tip)
        .is_some_and(|o| is_eligible(o, now, cooldown))
    {
        return CooldownDecision::Advance;
    }
    // tip は適用不可。熟成済み観測 commit への fallback を探す。
    let head_seen: Option<SystemTime> = current_head
        .and_then(|h| observed.iter().find(|o| o.commit == h))
        .and_then(|o| parse_rfc3339_utc(&o.first_seen));
    let Some(head_seen) = head_seen else {
        // 現 HEAD の観測時刻が分からない = fallback が前進か後退か判定不能。
        // 後退 (downgrade) のリスクを取らず現状維持。
        return CooldownDecision::Hold { fallback: None };
    };
    let fallback = observed
        .iter()
        .filter(|o| o.commit != tip && Some(o.commit.as_str()) != current_head)
        .filter(|o| is_eligible(o, now, cooldown))
        .filter_map(|o| parse_rfc3339_utc(&o.first_seen).map(|seen| (seen, o)))
        .filter(|(seen, _)| *seen > head_seen)
        .max_by_key(|(seen, _)| *seen)
        .map(|(_, o)| o.commit.clone());
    CooldownDecision::Hold { fallback }
}

/// 観測履歴を間引く。残すもの:
/// - 未熟成 (pending) の全 entry — これらは将来の fallback / Advance 候補
/// - 熟成済みのうち最も新しく観測された 1 件 — それより古い熟成済みは
///   fallback 選定で二度と勝てないので不要
/// - `keep` (現 HEAD) の entry — decide() の前進/後退比較の基準点
///
/// その上で `MAX_OBSERVED` 件に cap する (観測の古い順に drop)。
pub fn prune(
    observed: &mut Vec<ObservedCommit>,
    keep: Option<&str>,
    now: SystemTime,
    cooldown: Duration,
) {
    let newest_eligible: Option<String> = observed
        .iter()
        .filter(|o| is_eligible(o, now, cooldown))
        .max_by_key(|o| parse_rfc3339_utc(&o.first_seen))
        .map(|o| o.commit.clone());
    observed.retain(|o| {
        !is_eligible(o, now, cooldown)
            || Some(o.commit.as_str()) == newest_eligible.as_deref()
            || Some(o.commit.as_str()) == keep
    });
    if observed.len() > MAX_OBSERVED {
        observed.sort_by_key(|o| parse_rfc3339_utc(&o.first_seen));
        let excess = observed.len() - MAX_OBSERVED;
        observed.drain(..excess);
    }
}

/// `options.cooldown` / `[[plugins]] cooldown` の生文字列を Duration に解決する。
///
/// - 未指定 (None) → `DEFAULT_COOLDOWN` (1d。**デフォルト ON**: 明示的に
///   `"0"` で無効化しない限り cooldown を効かせる。pnpm 11 と同じ方針)
/// - `"0"` → 無効化の明示
/// - パース失敗 → warn を出して **1d にフォールバック** (安全機構なので
///   fail-open にしない; fetch_interval とは逆方向の倒し方)
pub fn resolve_cooldown(raw: Option<&str>) -> Duration {
    match raw {
        None => DEFAULT_COOLDOWN,
        Some(s) => match crate::fetch_state::parse_duration(s) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "\u{26a0} cooldown: {} — falling back to 1d (fail-closed)",
                    e
                );
                DEFAULT_COOLDOWN
            }
        },
    }
}

/// plugin 単位の実効 cooldown。per-plugin 指定が global を上書きする
/// (per-plugin `"0"` で個別 opt-out も可能)。
pub fn effective_cooldown(global: Option<&str>, per_plugin: Option<&str>) -> Duration {
    resolve_cooldown(per_plugin.or(global))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;
    use tempfile::tempdir;

    fn t0() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// `t0()` から `ago` 前に初観測した entry を作る。
    fn obs(commit: &str, ago: Duration) -> ObservedCommit {
        ObservedCommit {
            commit: commit.to_string(),
            first_seen: format_rfc3339_utc(t0() - ago),
            committed_at: None,
        }
    }

    fn obs_with_commit_date(
        commit: &str,
        seen_ago: Duration,
        committed_ago: Duration,
    ) -> ObservedCommit {
        ObservedCommit {
            committed_at: Some(format_rfc3339_utc(t0() - committed_ago)),
            ..obs(commit, seen_ago)
        }
    }

    // ───── is_eligible ─────

    #[test]
    fn test_eligible_when_first_seen_older_than_cooldown() {
        assert!(is_eligible(&obs("a", 2 * DAY), t0(), DAY));
    }

    #[test]
    fn test_not_eligible_when_first_seen_within_cooldown() {
        assert!(!is_eligible(
            &obs("a", Duration::from_secs(3600)),
            t0(),
            DAY
        ));
    }

    #[test]
    fn test_eligible_when_commit_date_old_even_if_just_seen() {
        // 休眠 repo: 60 日前の commit を今日初観測 → 即適用してよい。
        let o = obs_with_commit_date("a", Duration::ZERO, 60 * DAY);
        assert!(is_eligible(&o, t0(), DAY));
    }

    #[test]
    fn test_not_eligible_when_both_fresh() {
        let o = obs_with_commit_date("a", Duration::ZERO, Duration::from_secs(60));
        assert!(!is_eligible(&o, t0(), DAY));
    }

    #[test]
    fn test_not_eligible_on_future_timestamp() {
        // 時計逆行 (first_seen が未来) は安全側 = 未熟成扱い。
        let o = ObservedCommit {
            commit: "a".into(),
            first_seen: format_rfc3339_utc(t0() + DAY),
            committed_at: None,
        };
        assert!(!is_eligible(&o, t0(), DAY));
    }

    #[test]
    fn test_not_eligible_on_malformed_timestamp() {
        let o = ObservedCommit {
            commit: "a".into(),
            first_seen: "not-a-timestamp".into(),
            committed_at: Some("also-bad".into()),
        };
        assert!(!is_eligible(&o, t0(), DAY));
    }

    // ───── observe ─────

    #[test]
    fn test_observe_adds_new_commit() {
        let mut v = Vec::new();
        assert!(observe(&mut v, "abc", None, t0()));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].commit, "abc");
        assert_eq!(v[0].first_seen, format_rfc3339_utc(t0()));
    }

    #[test]
    fn test_observe_does_not_reset_first_seen() {
        // 再観測で first_seen を更新すると永遠に熟成しない — 絶対に上書きしない。
        let mut v = vec![obs("abc", DAY)];
        assert!(!observe(&mut v, "abc", None, t0()));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].first_seen, format_rfc3339_utc(t0() - DAY));
    }

    #[test]
    fn test_observe_records_commit_date() {
        let mut v = Vec::new();
        observe(&mut v, "abc", Some(t0() - 3 * DAY), t0());
        assert_eq!(v[0].committed_at, Some(format_rfc3339_utc(t0() - 3 * DAY)));
    }

    // ───── decide ─────

    #[test]
    fn test_decide_zero_cooldown_always_advances() {
        let got = decide(&[], "tip", Some("head"), t0(), Duration::ZERO);
        assert_eq!(got, CooldownDecision::Advance);
    }

    #[test]
    fn test_decide_noop_when_tip_is_current_head() {
        let got = decide(&[], "same", Some("same"), t0(), DAY);
        assert_eq!(got, CooldownDecision::Advance);
    }

    #[test]
    fn test_decide_advances_when_tip_matured() {
        let observed = vec![obs("head", 5 * DAY), obs("tip", 2 * DAY)];
        let got = decide(&observed, "tip", Some("head"), t0(), DAY);
        assert_eq!(got, CooldownDecision::Advance);
    }

    #[test]
    fn test_decide_advances_when_tip_commit_date_is_old() {
        // 休眠 repo の初回 update: tip は初観測だが commit 自体が古い → 即適用。
        let observed = vec![
            obs("head", 30 * DAY),
            obs_with_commit_date("tip", Duration::ZERO, 60 * DAY),
        ];
        let got = decide(&observed, "tip", Some("head"), t0(), DAY);
        assert_eq!(got, CooldownDecision::Advance);
    }

    #[test]
    fn test_decide_holds_fresh_tip_without_candidates() {
        let observed = vec![obs("head", 5 * DAY), obs("tip", Duration::from_secs(60))];
        let got = decide(&observed, "tip", Some("head"), t0(), DAY);
        assert_eq!(got, CooldownDecision::Hold { fallback: None });
    }

    #[test]
    fn test_decide_holds_unobserved_tip() {
        // 観測履歴に無い tip = 初観測前 → 当然 Hold (caller は observe 済みの
        // はずだが、未観測でも安全側に倒れることを保証する)。
        let observed = vec![obs("head", 5 * DAY)];
        let got = decide(&observed, "tip", Some("head"), t0(), DAY);
        assert_eq!(got, CooldownDecision::Hold { fallback: None });
    }

    #[test]
    fn test_decide_falls_back_to_matured_newer_commit() {
        // アクティブ repo: tip は若いが、3 日前に観測した tip 候補は熟成済み →
        // そこまで前進する (cooldown 分遅れの追従)。
        let observed = vec![
            obs("head", 10 * DAY),
            obs("mid", 3 * DAY),
            obs("tip", Duration::from_secs(60)),
        ];
        let got = decide(&observed, "tip", Some("head"), t0(), DAY);
        assert_eq!(
            got,
            CooldownDecision::Hold {
                fallback: Some("mid".to_string())
            }
        );
    }

    #[test]
    fn test_decide_fallback_picks_newest_matured() {
        let observed = vec![
            obs("head", 10 * DAY),
            obs("older", 5 * DAY),
            obs("newer", 2 * DAY),
            obs("tip", Duration::from_secs(60)),
        ];
        let got = decide(&observed, "tip", Some("head"), t0(), DAY);
        assert_eq!(
            got,
            CooldownDecision::Hold {
                fallback: Some("newer".to_string())
            }
        );
    }

    #[test]
    fn test_decide_no_fallback_when_candidate_is_current_head() {
        let observed = vec![obs("head", 5 * DAY), obs("tip", Duration::from_secs(60))];
        let got = decide(&observed, "tip", Some("head"), t0(), DAY);
        assert_eq!(got, CooldownDecision::Hold { fallback: None });
    }

    #[test]
    fn test_decide_no_fallback_when_head_unobserved() {
        // 現 HEAD の観測時刻が無い → fallback が後退かもしれないので動かない。
        let observed = vec![obs("matured", 5 * DAY), obs("tip", Duration::from_secs(60))];
        let got = decide(&observed, "tip", Some("unknown-head"), t0(), DAY);
        assert_eq!(got, CooldownDecision::Hold { fallback: None });
    }

    #[test]
    fn test_decide_no_fallback_older_than_head() {
        // 熟成済みでも現 HEAD より前に観測されたもの (= 過去) へは巻き戻さない。
        let observed = vec![
            obs("ancient", 20 * DAY),
            obs("head", 10 * DAY),
            obs("tip", Duration::from_secs(60)),
        ];
        let got = decide(&observed, "tip", Some("head"), t0(), DAY);
        assert_eq!(got, CooldownDecision::Hold { fallback: None });
    }

    // ───── prune ─────

    #[test]
    fn test_prune_keeps_pending_and_newest_matured() {
        let mut v = vec![
            obs("old-matured", 10 * DAY),
            obs("new-matured", 2 * DAY),
            obs("pending", Duration::from_secs(60)),
        ];
        prune(&mut v, None, t0(), DAY);
        let names: Vec<_> = v.iter().map(|o| o.commit.as_str()).collect();
        assert!(names.contains(&"new-matured"));
        assert!(names.contains(&"pending"));
        assert!(!names.contains(&"old-matured"));
    }

    #[test]
    fn test_prune_keeps_current_head_even_if_old() {
        let mut v = vec![
            obs("head", 10 * DAY),
            obs("new-matured", 2 * DAY),
            obs("pending", Duration::from_secs(60)),
        ];
        prune(&mut v, Some("head"), t0(), DAY);
        let names: Vec<_> = v.iter().map(|o| o.commit.as_str()).collect();
        assert!(names.contains(&"head"), "current head must survive pruning");
        assert!(names.contains(&"new-matured"));
        assert!(names.contains(&"pending"));
    }

    #[test]
    fn test_prune_caps_total_entries() {
        let mut v: Vec<ObservedCommit> = (0..(MAX_OBSERVED + 50))
            .map(|i| obs(&format!("c{}", i), Duration::from_secs(i as u64)))
            .collect();
        prune(&mut v, None, t0(), 365 * DAY); // 全部 pending 扱いで cap だけ効かせる
        assert_eq!(v.len(), MAX_OBSERVED);
        // 観測の新しい (= ago が小さい) ものが残る
        assert!(v.iter().any(|o| o.commit == "c0"));
        assert!(
            !v.iter()
                .any(|o| o.commit == format!("c{}", MAX_OBSERVED + 49))
        );
    }

    // ───── resolve / effective cooldown ─────

    #[test]
    fn test_resolve_cooldown_unset_defaults_on() {
        // デフォルト ON: 未指定なら 1d が効く (`"0"` で明示無効化しない限り)。
        assert_eq!(resolve_cooldown(None), DEFAULT_COOLDOWN);
    }

    #[test]
    fn test_resolve_cooldown_zero_disables() {
        assert_eq!(resolve_cooldown(Some("0")), Duration::ZERO);
    }

    #[test]
    fn test_resolve_cooldown_parses_days() {
        assert_eq!(resolve_cooldown(Some("3d")), Duration::from_secs(3 * 86400));
    }

    #[test]
    fn test_resolve_cooldown_bad_input_fails_closed() {
        // 安全機構なので設定ミスは「無効化」ではなく 1d に倒す。
        assert_eq!(resolve_cooldown(Some("garbage")), DEFAULT_COOLDOWN);
    }

    #[test]
    fn test_effective_cooldown_unset_everywhere_defaults_on() {
        // global も per-plugin も未指定 → デフォルト ON の 1d。
        assert_eq!(effective_cooldown(None, None), DEFAULT_COOLDOWN);
    }

    #[test]
    fn test_effective_cooldown_plugin_overrides_global() {
        assert_eq!(
            effective_cooldown(Some("1d"), Some("7d")),
            Duration::from_secs(7 * 86400)
        );
    }

    #[test]
    fn test_effective_cooldown_plugin_zero_opts_out() {
        assert_eq!(effective_cooldown(Some("1d"), Some("0")), Duration::ZERO);
    }

    #[test]
    fn test_effective_cooldown_plugin_zero_opts_out_of_default() {
        // global 未指定 (= デフォルト ON) でも、plugin `"0"` で個別 opt-out できる。
        assert_eq!(effective_cooldown(None, Some("0")), Duration::ZERO);
    }

    #[test]
    fn test_effective_cooldown_falls_back_to_global() {
        assert_eq!(
            effective_cooldown(Some("1d"), None),
            Duration::from_secs(86400)
        );
    }

    // ───── humanize helpers ─────

    #[test]
    fn test_humanize_age_buckets() {
        let stamp = |ago: Duration| format_rfc3339_utc(t0() - ago);
        assert_eq!(humanize_age(&stamp(Duration::from_secs(30)), t0()), "30s");
        assert_eq!(
            humanize_age(&stamp(Duration::from_secs(5 * 60)), t0()),
            "5m"
        );
        assert_eq!(
            humanize_age(&stamp(Duration::from_secs(3 * 3600)), t0()),
            "3h"
        );
        assert_eq!(humanize_age(&stamp(2 * DAY), t0()), "2d");
    }

    #[test]
    fn test_humanize_age_degrades_on_bad_or_future_input() {
        assert_eq!(humanize_age("garbage", t0()), "?");
        assert_eq!(humanize_age(&format_rfc3339_utc(t0() + DAY), t0()), "?");
    }

    #[test]
    fn test_humanize_duration_picks_largest_clean_unit() {
        assert_eq!(humanize_duration(DAY), "1d");
        assert_eq!(humanize_duration(Duration::from_secs(12 * 3600)), "12h");
        assert_eq!(humanize_duration(Duration::from_secs(90 * 60)), "90m");
        assert_eq!(humanize_duration(Duration::ZERO), "0");
    }

    // ───── load / save / persistence ─────

    fn mk_entry(name: &str) -> CooldownEntry {
        CooldownEntry {
            name: name.to_string(),
            url: format!("owner/{}", name),
            observed: vec![obs("abc", DAY)],
        }
    }

    #[test]
    fn test_load_missing_returns_default() {
        let dir = tempdir().unwrap();
        let state = CooldownState::load(&dir.path().join("nonexistent.json"));
        assert_eq!(state.version, CURRENT_VERSION);
        assert!(state.entries.is_empty());
    }

    #[test]
    fn test_load_malformed_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json at all {{{{").unwrap();
        assert!(CooldownState::load(&path).entries.is_empty());
    }

    #[test]
    fn test_load_rejects_future_schema_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("future.json");
        std::fs::write(
            &path,
            r#"{"version":99,"entries":[{"name":"x","url":"o/x","observed":[]}]}"#,
        )
        .unwrap();
        let state = CooldownState::load(&path);
        assert!(state.entries.is_empty());
        assert_eq!(state.version, CURRENT_VERSION);
    }

    #[test]
    fn test_save_then_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cooldown_state.json");
        let mut state = CooldownState::default();
        state.entries.push(mk_entry("b"));
        state.entries.push(mk_entry("a"));
        state.save(&path).unwrap();

        let loaded = CooldownState::load(&path);
        assert_eq!(loaded.entries.len(), 2);
        // name で安定 sort されて書かれる
        assert_eq!(loaded.entries[0].name, "a");
        assert_eq!(loaded.entries[1].name, "b");
        assert_eq!(loaded.entries[0].observed[0].commit, "abc");
    }

    #[test]
    fn test_upsert_replaces_existing_entry() {
        let mut state = CooldownState::default();
        state.upsert(mk_entry("a"));
        let mut replacement = mk_entry("a");
        replacement.observed.push(obs("def", Duration::ZERO));
        state.upsert(replacement);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].observed.len(), 2);
    }

    #[test]
    fn test_retain_by_names_drops_orphans() {
        let mut state = CooldownState::default();
        state.upsert(mk_entry("a"));
        state.upsert(mk_entry("b"));
        let mut keep = std::collections::HashSet::new();
        keep.insert("a".to_string());
        state.retain_by_names(&keep);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].name, "a");
    }
}
