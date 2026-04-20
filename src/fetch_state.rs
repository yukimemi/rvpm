//! `<cache_root>/fetch_state.json` の read/write と
//! 「この plugin を今 fetch するか」の pure 判定ロジック。
//!
//! 背景:
//! - `rvpm sync` は毎回 plugin ごとに `fetch_impl` を呼んでいたが、held-back
//!   サマリ (#68) 導入後は「lockfile pin が remote 最新と合致している場合」でも
//!   network round trip が発生して体感で重い。
//! - 素朴に「HEAD == pin なら fetch 省略」にすると、`rvpm sync` しか使わない
//!   ユーザーが remote の進化に永遠に気付けない = #68 で潰したはずの罠が別形で
//!   再発する。
//! - そこで **plugin 単位の最終 fetch 時刻** を記録して、staleness window (デフォ
//!   ルト 6h) 以内は fetch を省略、window 超過なら通常フローに戻る。window 超過
//!   時の full flow が held-back サマリを再評価してくれるので罠にはならない。
//!
//! スキーマ:
//! ```json
//! { "version": 1,
//!   "entries": [
//!     { "name": "snacks.nvim", "url": "folke/snacks.nvim",
//!       "last_fetched": "2026-04-19T12:34:56Z" }
//!   ] }
//! ```
//!
//! - **場所**: `<cache_root>/fetch_state.json` (ephemeral cache 側に置く。
//!   dotfile 管理する `rvpm.lock` とは区別)。
//! - **resilience**: malformed / missing → empty state (= 全プラグイン fetch)
//!   にフォールバック。ユーザー操作は止めない。
//! - **schema version**: 未対応バージョンは empty 扱い (lockfile と同じパターン)。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::update_log::{format_rfc3339_utc, parse_rfc3339_utc};

/// 現行スキーマバージョン。壊すときは bump + migration を足す。
pub const CURRENT_VERSION: u32 = 1;

/// fetch_state のルート構造。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchState {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<FetchEntry>,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}

impl Default for FetchState {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            entries: Vec::new(),
        }
    }
}

/// 1 プラグイン分の fetch 時刻エントリ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchEntry {
    /// `Plugin::display_name()` 由来の lookup キー。
    pub name: String,
    /// config.toml に書かれた URL。同じ name で別 repo に差し替えられた時の検知用
    /// (lockfile と同じ思想)。
    pub url: String,
    /// 最後に成功した fetch の時刻。RFC3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`)。
    pub last_fetched: String,
}

impl FetchState {
    /// `path` から読み出す。
    /// - 存在しない → `Default` (empty)
    /// - パース失敗 / version mismatch → warn を出して `Default`
    pub fn load(path: &Path) -> Self {
        let content = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                eprintln!(
                    "\u{26a0} fetch_state: failed to read {}: {} (treating as empty)",
                    path.display(),
                    e
                );
                return Self::default();
            }
        };
        match serde_json::from_str::<FetchState>(&content) {
            Ok(s) if s.version == CURRENT_VERSION => s,
            Ok(s) => {
                eprintln!(
                    "\u{26a0} fetch_state: unsupported version {} in {} (expected {}; treating as empty)",
                    s.version,
                    path.display(),
                    CURRENT_VERSION
                );
                Self::default()
            }
            Err(e) => {
                eprintln!(
                    "\u{26a0} fetch_state: failed to parse {}: {} (treating as empty)",
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
        let body = serde_json::to_string_pretty(self).context("serialize fetch_state")?;
        let parent = path.parent().unwrap_or(Path::new("."));
        let tmp = tempfile::Builder::new()
            .prefix(".rvpm-fetch-state-")
            .suffix(".tmp")
            .tempfile_in(parent)
            .with_context(|| format!("create tempfile in {}", parent.display()))?;
        std::fs::write(tmp.path(), body.as_bytes())
            .with_context(|| format!("write tempfile {}", tmp.path().display()))?;
        tmp.persist(path)
            .map_err(|e| anyhow::anyhow!("rename tempfile to {}: {}", path.display(), e))?;
        Ok(())
    }

    pub fn find(&self, name: &str) -> Option<&FetchEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn upsert(&mut self, entry: FetchEntry) {
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

/// CLI から fetch 判定を上書きするモード。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// デフォルト: `fetch_interval` で staleness 判定。
    Auto,
    /// `--refresh`: window 無視で常に fetch。
    Force,
    /// `--no-refresh`: window 無視で常にスキップ (offline モード)。
    Skip,
}

/// この plugin を今 fetch すべきか判定する pure function。
///
/// 決定表:
/// - `Force` → 常に true
/// - `Skip` → 常に false
/// - `Auto`:
///   - `interval == ZERO` (= cache 無効) → true
///   - `last_fetched` が無い / パース不能 → true (安全側)
///   - `now - last_fetched >= interval` → true
///   - それ以外 → false
///
/// 時計が逆行した場合 (`now < last_fetched`) は fetch に倒す (cache が壊れている
/// 可能性を優先)。
pub fn should_fetch(
    last_fetched: Option<&str>,
    now: SystemTime,
    interval: Duration,
    mode: RefreshMode,
) -> bool {
    match mode {
        RefreshMode::Force => return true,
        RefreshMode::Skip => return false,
        RefreshMode::Auto => {}
    }
    if interval == Duration::ZERO {
        return true;
    }
    let Some(last) = last_fetched else {
        return true;
    };
    let Some(then) = parse_rfc3339_utc(last) else {
        return true;
    };
    match now.duration_since(then) {
        Ok(elapsed) => elapsed >= interval,
        Err(_) => true,
    }
}

/// 現在時刻を RFC3339 UTC 文字列で返す (persistence 用)。
pub fn now_rfc3339() -> String {
    format_rfc3339_utc(SystemTime::now())
}

/// Humantime-lite な Duration パーサ。`"6h" / "30m" / "1d" / "45s" / "0"` を受ける。
///
/// 受け付ける単位:
/// - `s` - 秒
/// - `m` - 分
/// - `h` - 時間
/// - `d` - 日
///
/// `"0"` だけ特別扱いで `Duration::ZERO` (= cache 無効化の慣用表現)。
/// 数値オーバーフロー時は `u64` 秒上限 (十分大きい) でクリップする。
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    let (num_part, unit) = split_number_unit(s).ok_or_else(|| {
        format!(
            "invalid duration: {:?} (expected e.g. \"6h\", \"30m\", \"1d\", \"45s\", \"0\")",
            s
        )
    })?;
    let n: u64 = num_part
        .parse()
        .map_err(|_| format!("invalid duration number: {:?}", num_part))?;
    let mult: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 60 * 60 * 24,
        other => return Err(format!("unknown duration unit: {:?} (use s/m/h/d)", other)),
    };
    let secs = n.saturating_mul(mult);
    Ok(Duration::from_secs(secs))
}

fn split_number_unit(s: &str) -> Option<(&str, &str)> {
    let boundary = s.find(|c: char| !c.is_ascii_digit())?;
    if boundary == 0 {
        return None;
    }
    Some((&s[..boundary], &s[boundary..]))
}

/// `options.fetch_interval` (ユーザー設定) を Duration に解決する。
/// 未設定 → 6 時間。パース失敗 → warn を出してデフォルトに fallback (resilience)。
pub fn resolve_fetch_interval(raw: Option<&str>) -> Duration {
    const DEFAULT: Duration = Duration::from_secs(6 * 60 * 60);
    match raw {
        None => DEFAULT,
        Some(s) => match parse_duration(s) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "\u{26a0} options.fetch_interval: {} — falling back to 6h",
                    e
                );
                DEFAULT
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;
    use tempfile::tempdir;

    fn mk(name: &str, last: &str) -> FetchEntry {
        FetchEntry {
            name: name.to_string(),
            url: format!("owner/{}", name),
            last_fetched: last.to_string(),
        }
    }

    // ───── load / save / persistence ─────

    #[test]
    fn test_load_missing_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let state = FetchState::load(&path);
        assert_eq!(state.version, CURRENT_VERSION);
        assert!(state.entries.is_empty());
    }

    #[test]
    fn test_load_malformed_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "this is not valid json =====").unwrap();
        let state = FetchState::load(&path);
        assert!(state.entries.is_empty());
    }

    #[test]
    fn test_save_then_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("fetch_state.json");
        let mut state = FetchState::default();
        state.entries.push(mk("a", "2026-01-01T00:00:00Z"));
        state.entries.push(mk("b", "2026-01-02T00:00:00Z"));
        state.save(&path).unwrap();

        let loaded = FetchState::load(&path);
        assert_eq!(loaded.version, CURRENT_VERSION);
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.entries[0].name, "a");
        assert_eq!(loaded.entries[1].name, "b");
    }

    #[test]
    fn test_save_sorts_entries_by_name_for_stable_diffs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("fetch_state.json");
        let mut state = FetchState::default();
        state.entries.push(mk("zeta", "2026-01-03T00:00:00Z"));
        state.entries.push(mk("alpha", "2026-01-01T00:00:00Z"));
        state.entries.push(mk("mid", "2026-01-02T00:00:00Z"));
        state.save(&path).unwrap();

        let loaded = FetchState::load(&path);
        let names: Vec<_> = loaded.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn test_upsert_inserts_new_entry() {
        let mut state = FetchState::default();
        state.upsert(mk("a", "2026-01-01T00:00:00Z"));
        assert_eq!(state.entries.len(), 1);
    }

    #[test]
    fn test_upsert_replaces_existing_entry() {
        let mut state = FetchState::default();
        state.upsert(mk("a", "2026-01-01T00:00:00Z"));
        state.upsert(mk("a", "2026-02-01T00:00:00Z"));
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].last_fetched, "2026-02-01T00:00:00Z");
    }

    #[test]
    fn test_find_returns_matching_entry() {
        let mut state = FetchState::default();
        state.upsert(mk("a", "2026-01-01T00:00:00Z"));
        state.upsert(mk("b", "2026-01-02T00:00:00Z"));
        assert_eq!(
            state.find("b").map(|e| e.last_fetched.as_str()),
            Some("2026-01-02T00:00:00Z")
        );
        assert!(state.find("missing").is_none());
    }

    #[test]
    fn test_retain_by_names_drops_orphans() {
        let mut state = FetchState::default();
        state.upsert(mk("a", "2026-01-01T00:00:00Z"));
        state.upsert(mk("b", "2026-01-02T00:00:00Z"));
        state.upsert(mk("c", "2026-01-03T00:00:00Z"));
        let mut keep = std::collections::HashSet::new();
        keep.insert("a".into());
        keep.insert("c".into());
        state.retain_by_names(&keep);
        let names: Vec<_> = state.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"c"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn test_load_rejects_future_schema_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("future.json");
        std::fs::write(
            &path,
            r#"{"version":99,"entries":[{"name":"x","url":"o/x","last_fetched":"2026-01-01T00:00:00Z"}]}"#,
        )
        .unwrap();
        let state = FetchState::load(&path);
        assert!(
            state.entries.is_empty(),
            "future schema must degrade to empty state"
        );
        assert_eq!(state.version, CURRENT_VERSION);
    }

    // ───── should_fetch decision matrix ─────

    fn t0() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn test_should_fetch_force_always_true() {
        // Force overrides everything — even a freshly recorded pin.
        assert!(should_fetch(
            Some("2026-04-19T12:00:00Z"),
            t0(),
            Duration::from_secs(3600),
            RefreshMode::Force,
        ));
    }

    #[test]
    fn test_should_fetch_skip_always_false() {
        // Skip overrides everything — even a stale/absent pin.
        assert!(!should_fetch(
            None,
            t0(),
            Duration::from_secs(3600),
            RefreshMode::Skip,
        ));
    }

    #[test]
    fn test_should_fetch_auto_no_prior_entry_fetches() {
        assert!(should_fetch(
            None,
            t0(),
            Duration::from_secs(3600),
            RefreshMode::Auto,
        ));
    }

    #[test]
    fn test_should_fetch_auto_within_window_skips() {
        let then = t0() - Duration::from_secs(30 * 60); // 30 min ago
        let last = format_rfc3339_utc(then);
        assert!(!should_fetch(
            Some(&last),
            t0(),
            Duration::from_secs(6 * 3600), // 6h
            RefreshMode::Auto,
        ));
    }

    #[test]
    fn test_should_fetch_auto_outside_window_fetches() {
        let then = t0() - Duration::from_secs(7 * 3600); // 7h ago
        let last = format_rfc3339_utc(then);
        assert!(should_fetch(
            Some(&last),
            t0(),
            Duration::from_secs(6 * 3600),
            RefreshMode::Auto,
        ));
    }

    #[test]
    fn test_should_fetch_auto_zero_interval_disables_cache() {
        // fetch_interval = "0" means "always fetch" — cache disabled.
        let then = t0() - Duration::from_secs(1);
        let last = format_rfc3339_utc(then);
        assert!(should_fetch(
            Some(&last),
            t0(),
            Duration::ZERO,
            RefreshMode::Auto,
        ));
    }

    #[test]
    fn test_should_fetch_malformed_timestamp_falls_back_to_fetch() {
        assert!(should_fetch(
            Some("not-a-timestamp"),
            t0(),
            Duration::from_secs(3600),
            RefreshMode::Auto,
        ));
    }

    #[test]
    fn test_should_fetch_clock_backward_falls_back_to_fetch() {
        // last_fetched is in the future relative to `now` — treat cache as
        // untrusted and fetch.
        let future = t0() + Duration::from_secs(3600);
        let last = format_rfc3339_utc(future);
        assert!(should_fetch(
            Some(&last),
            t0(),
            Duration::from_secs(6 * 3600),
            RefreshMode::Auto,
        ));
    }

    // ───── parse_duration ─────

    #[test]
    fn test_parse_duration_accepts_hours() {
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(6 * 3600));
    }

    #[test]
    fn test_parse_duration_accepts_minutes() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(30 * 60));
    }

    #[test]
    fn test_parse_duration_accepts_days() {
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn test_parse_duration_accepts_seconds() {
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
    }

    #[test]
    fn test_parse_duration_zero_is_disable() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
    }

    #[test]
    fn test_parse_duration_trims_whitespace() {
        assert_eq!(
            parse_duration("  6h  ").unwrap(),
            Duration::from_secs(6 * 3600)
        );
    }

    #[test]
    fn test_parse_duration_rejects_empty() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("   ").is_err());
    }

    #[test]
    fn test_parse_duration_rejects_unknown_unit() {
        assert!(parse_duration("6w").is_err()); // weeks not supported
        assert!(parse_duration("6y").is_err());
    }

    #[test]
    fn test_parse_duration_rejects_no_unit() {
        assert!(parse_duration("6").is_err());
        assert!(parse_duration("60").is_err());
    }

    #[test]
    fn test_parse_duration_rejects_no_number() {
        assert!(parse_duration("h").is_err());
        assert!(parse_duration("d").is_err());
    }

    // ───── resolve_fetch_interval ─────

    #[test]
    fn test_resolve_fetch_interval_default_6h() {
        assert_eq!(resolve_fetch_interval(None), Duration::from_secs(6 * 3600));
    }

    #[test]
    fn test_resolve_fetch_interval_honors_user_setting() {
        assert_eq!(
            resolve_fetch_interval(Some("30m")),
            Duration::from_secs(30 * 60)
        );
    }

    #[test]
    fn test_resolve_fetch_interval_falls_back_on_bad_input() {
        // Bad user input should degrade to default, not crash the sync.
        assert_eq!(
            resolve_fetch_interval(Some("not-a-duration")),
            Duration::from_secs(6 * 3600)
        );
    }
}
