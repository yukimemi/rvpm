//! `rvpm log` の永続化と整形ロジック。
//!
//! `sync` / `update` / `add` の git 操作の結果を `<cache_root>/update_log.json`
//! に追記し、`rvpm log [query]` で人間可読に表示するための reusable な型と
//! pure function を提供する。
//!
//! 設計方針:
//! - persist 失敗 (disk full / 権限) は警告で continue (resilience 原則)。
//! - JSON 破損ファイルも `Default::default()` で扱い、ユーザーの操作を止めない。
//! - 整形 (相対時間、render_log) はすべて pure function にしてテスト容易性を確保。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::IconStyle;

/// 永続化するログのトップレベル。
///
/// `runs` は **新しいものを末尾** に追加し、`MAX_RUNS` を超えたら先頭から落とす。
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateLog {
    #[serde(default)]
    pub runs: Vec<RunRecord>,
}

/// 1 回の `sync` / `update` / `add` 実行の記録。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunRecord {
    /// RFC3339 (`YYYY-MM-DDTHH:MM:SSZ`) UTC。
    pub timestamp: String,
    /// `"sync"` / `"update"` / `"add"` のいずれか。
    pub command: String,
    pub changes: Vec<ChangeRecord>,
}

/// 1 プラグイン分の差分。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeRecord {
    /// プラグインの display_name (config.toml の name か、URL から推論)。
    pub name: String,
    /// プラグインの URL (config.toml に書かれた raw 値)。
    pub url: String,
    /// 操作前の HEAD commit hash (full)。新規 clone の場合 None。
    pub from: Option<String>,
    /// 操作後の HEAD commit hash (full)。
    pub to: String,
    /// `<from>..<to>` の commit subject 一覧 (新しい順)。
    pub subjects: Vec<String>,
    /// `subjects` のうち BREAKING と判定されたサブセット。
    pub breaking_subjects: Vec<String>,
    /// `<from>..<to>` で変更があった README/CHANGELOG/doc 系ファイル。
    pub doc_files_changed: Vec<String>,
}

/// 履歴上限。これより古い run は読み込み時 / 書き込み時に drop される。
pub const MAX_RUNS: usize = 20;

/// `--last N` のデフォルト値。
pub const DEFAULT_LAST: usize = 1;

// =====================================================================
// I/O — persistence (atomic write, malformed-tolerant load)
// =====================================================================

/// `update_log.json` を読み込む。ファイルが無い / 壊れている場合は `Default` を返し、
/// stderr に warning を出す (resilience 原則: ユーザー操作は止めない)。
pub fn load_log(path: &Path) -> UpdateLog {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return UpdateLog::default(),
        Err(e) => {
            eprintln!(
                "\u{26a0} update_log: failed to read {}: {} (treating as empty)",
                path.display(),
                e
            );
            return UpdateLog::default();
        }
    };
    match serde_json::from_str::<UpdateLog>(&content) {
        Ok(mut log) => {
            // 古いバージョン由来の上限超過は読み込み時にも cap する。
            cap_runs(&mut log);
            log
        }
        Err(e) => {
            eprintln!(
                "\u{26a0} update_log: failed to parse {}: {} (treating as empty)",
                path.display(),
                e
            );
            UpdateLog::default()
        }
    }
}

/// `MAX_RUNS` を超えていれば先頭から切り捨てる (新しいもの = 末尾を残す)。
pub fn cap_runs(log: &mut UpdateLog) {
    if log.runs.len() > MAX_RUNS {
        let drop = log.runs.len() - MAX_RUNS;
        log.runs.drain(0..drop);
    }
}

/// 1 回分の run を追記する。**空の changes なら何もしない** (file に書かない)。
///
/// `MAX_RUNS` cap があるので空 run (HEAD が動かなかった no-op `sync`/`update`)
/// を記録すると、有用な履歴を押し出す恐れがある。`rvpm log` は元々空 run を
/// 表示しないので、そもそも記録する意味が無い。
///
/// 書き込みは tempfile + rename の atomic write。
/// 失敗しても呼び出し元の操作を壊さないよう、エラーは Ok 扱いで eprintln する責務は
/// caller 側に委ねる (Result を返すことでテスト可能性は保つ)。
pub fn record_run(path: &Path, command: &str, changes: Vec<ChangeRecord>) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut log = load_log(path);
    let timestamp = format_rfc3339_utc(SystemTime::now());
    log.runs.push(RunRecord {
        timestamp,
        command: command.to_string(),
        changes,
    });
    cap_runs(&mut log);
    save_log(path, &log)
}

/// JSON を atomic write (tempfile + rename) で保存する。
pub fn save_log(path: &Path, log: &UpdateLog) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(log).context("serialize update_log")?;
    let parent = path.parent().unwrap_or(Path::new("."));
    // tempfile を同一ディレクトリに作成 (cross-device rename を回避)。
    let tmp = tempfile::Builder::new()
        .prefix(".rvpm-update-log-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("create tempfile in {}", parent.display()))?;
    std::fs::write(tmp.path(), json.as_bytes())
        .with_context(|| format!("write tempfile {}", tmp.path().display()))?;
    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("rename tempfile to {}: {}", path.display(), e))?;
    Ok(())
}

// =====================================================================
// Time helpers
// =====================================================================

/// SystemTime を `YYYY-MM-DDTHH:MM:SSZ` (UTC, RFC3339) 形式に整形する。
/// chrono / time crate を導入したくないので civil-from-days を手で実装。
pub fn format_rfc3339_utc(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix_secs(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

/// RFC3339 (UTC, `Z` suffix) を SystemTime に戻す。失敗したら None。
/// `+00:00` も受け付ける。それ以外の timezone は今回は扱わない。
pub fn parse_rfc3339_utc(s: &str) -> Option<SystemTime> {
    // 期待: "YYYY-MM-DDTHH:MM:SSZ" (20 chars) or with "+00:00" (25 chars)
    let s = s.trim();
    let core = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    // ASCII 前提の byte-range 切り出しをするので、ASCII であることを先に検証。
    // 非 ASCII の multi-byte 文字が 19 byte に偶然フィットすると `core[0..4]`
    // が char boundary で panic するため。
    if core.len() != 19 || !core.is_ascii() || core.as_bytes()[10] != b'T' {
        return None;
    }
    let y: i64 = core[0..4].parse().ok()?;
    let mo: u32 = core[5..7].parse().ok()?;
    let d: u32 = core[8..10].parse().ok()?;
    let h: u32 = core[11..13].parse().ok()?;
    let mi: u32 = core[14..16].parse().ok()?;
    let s_: u32 = core[17..19].parse().ok()?;
    let secs = unix_secs_from_civil(y, mo, d, h, mi, s_)?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// 現在時刻からの相対表示 ("Just now" / "5 minutes ago" / "3 hours ago" / "2 days ago" /
/// 7 日超は absolute date "YYYY-MM-DD")。`now` は依存性注入でテスト可能にする。
pub fn format_relative(then: SystemTime, now: SystemTime) -> String {
    let delta = now.duration_since(then).unwrap_or(Duration::ZERO);
    let secs = delta.as_secs();
    if secs < 60 {
        return "Just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{} minute{} ago", mins, if mins == 1 { "" } else { "s" });
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{} hour{} ago", hours, if hours == 1 { "" } else { "s" });
    }
    let days = hours / 24;
    if days <= 7 {
        return format!("{} day{} ago", days, if days == 1 { "" } else { "s" });
    }
    // 7 日超は絶対日付
    let secs_i64 = then
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, _, _, _) = civil_from_unix_secs(secs_i64);
    format!("{:04}-{:02}-{:02}", y, mo, d)
}

// civil <-> unix days conversions (Howard Hinnant's algorithm)
// https://howardhinnant.github.io/date_algorithms.html
fn civil_from_unix_secs(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400) as u32;
    let h = time / 3600;
    let mi = (time % 3600) / 60;
    let s = time % 60;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, mi, s)
}

fn unix_secs_from_civil(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<i64> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let m = mo as u64;
    let d_u = d as u64;
    let doy = (153 * if m > 2 { m - 3 } else { m + 9 } + 2) / 5 + d_u - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    Some(days * 86_400 + h as i64 * 3600 + mi as i64 * 60 + s as i64)
}

// =====================================================================
// BREAKING CHANGE detection
// =====================================================================

/// Conventional Commits の bang 形式 (`feat!:`, `fix(scope)!:`) もしくは
/// body の `BREAKING CHANGE:` / `BREAKING-CHANGE:` 行 (case-insensitive) を
/// 検出する。subject / body のどちらかにマッチすれば true。
///
/// 用途上 `body` には full commit message body (subject 含まない) が来る前提
/// だが、subject を含んでいても誤検出は起きない。
pub fn is_breaking(subject: &str, body: &str) -> bool {
    if subject_indicates_breaking(subject) {
        return true;
    }
    body_indicates_breaking(body)
}

/// `<type>!:` / `<type>(<scope>)!:` の形を検出する。
/// 大文字小文字どちらでも可 (`Feat!:` などツールによっては大文字始まり)。
fn subject_indicates_breaking(subject: &str) -> bool {
    let bytes = subject.as_bytes();
    let mut i = 0;
    // type: 1 文字以上の ASCII alpha
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == 0 {
        return false;
    }
    // 任意の `(scope)`
    if i < bytes.len() && bytes[i] == b'(' {
        let start = i;
        i += 1;
        while i < bytes.len() && bytes[i] != b')' {
            i += 1;
        }
        if i >= bytes.len() {
            return false; // 未閉じ括弧
        }
        i += 1; // skip ')'
        // 空 scope `()` は弾く
        if i - start <= 2 {
            return false;
        }
    }
    // ! の直後に `:`
    i + 1 < bytes.len() && bytes[i] == b'!' && bytes[i + 1] == b':'
}

fn body_indicates_breaking(body: &str) -> bool {
    for line in body.lines() {
        let trimmed = line.trim_start();
        // "BREAKING CHANGE:" / "BREAKING-CHANGE:" はどちらも 16 byte (ASCII)。
        // byte-range で先頭 16 byte を直接取り、ASCII の大文字小文字無視比較する。
        // `get(..16)` は char boundary に着地しない場合 None を返すので panic しない。
        if let Some(head) = trimmed.get(..16)
            && (head.eq_ignore_ascii_case("breaking change:")
                || head.eq_ignore_ascii_case("breaking-change:"))
        {
            return true;
        }
    }
    false
}

// =====================================================================
// Render (plain text)
// =====================================================================

/// `rvpm log --diff` が埋め込む patch の lookup キー。
///
/// - `url` で plugin を一意化 (同じ `name` が別 owner で使われていても衝突しない)
/// - `(from, to)` の commit range で run を区別 (`--last 2 --diff` で同じ
///   plugin の同じ doc file が複数 run に現れても、各 run の patch が別々に
///   lookup できる)
/// - `file` で doc file ごとに区別
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiffKey {
    pub url: String,
    pub from: String,
    pub to: String,
    pub file: String,
}

/// `rvpm log` の表示オプション。CLI フラグから組み立てる。
#[derive(Debug, Clone)]
pub struct LogRenderOptions<'a> {
    /// 表示する run 数 (新しい順から)。
    pub last: usize,
    /// 名前フィルタ (case-insensitive substring)。
    pub query: Option<&'a str>,
    /// `--full`: subject だけでなく commit body も出す予定 (現状は body を永続化
    /// していないので subject のみ。フィールドは将来の拡張用に予約)。
    #[allow(dead_code)]
    pub full: bool,
    /// `--diff`: doc files の patch を埋め込む (caller が Vec<String> を渡す)。
    pub diff: bool,
    /// `DiffKey` → diff text の事前取得済みマップ。`--diff` が false なら空。
    pub diffs: std::collections::HashMap<DiffKey, String>,
    /// アイコンスタイル (BREAKING マーカーの装飾用)。
    pub icons: IconStyle,
    /// 「今」: テスト容易性のため呼び出し側から注入。
    pub now: SystemTime,
}

/// プラグイン名表示の最小幅 (パディング)。
pub const PLUGIN_NAME_PAD: usize = 26;

/// commit hash を 7 文字の short hash に縮める。
pub fn short_hash(h: &str) -> String {
    let take = 7.min(h.len());
    h[..take].to_string()
}

/// `runs` を新しい順 (= `runs` の末尾から) に取り出して整形する。
///
/// 出力は `\n` 終端、改行を `\r\n` 化したりはしない。
pub fn render_log(log: &UpdateLog, opts: &LogRenderOptions<'_>) -> String {
    let mut out = String::new();
    out.push_str("rvpm log \u{2014} recent updates\n\n");

    if log.runs.is_empty() {
        out.push_str("(no runs recorded yet)\n");
        return out;
    }

    let breaking_marker = breaking_marker_for(opts.icons);
    // query の lowercase は 1 回だけ評価する (ループ内で plugin ごとに
    // `.to_lowercase()` を再計算するとプラグイン数に比例して無駄)。
    let query_lower: Option<String> = opts.query.map(|q| q.to_lowercase());

    // 新しい順、`last` 件まで。query で絞り込んだ後の changes が空ならその run は表示しない。
    let mut shown = 0;
    for run in log.runs.iter().rev() {
        if shown >= opts.last {
            break;
        }
        let filtered: Vec<&ChangeRecord> = run
            .changes
            .iter()
            .filter(|c| match &query_lower {
                Some(q) => c.name.to_lowercase().contains(q.as_str()),
                None => true,
            })
            .collect();
        if filtered.is_empty() {
            continue;
        }
        shown += 1;

        let when = parse_rfc3339_utc(&run.timestamp).unwrap_or(opts.now);
        let rel = format_relative(when, opts.now);
        out.push_str(&format!(
            "# {} \u{2014} {} ({} {})\n",
            rel,
            run.command,
            filtered.len(),
            if filtered.len() == 1 {
                "plugin"
            } else {
                "plugins"
            },
        ));

        for change in filtered {
            render_change(&mut out, change, opts, breaking_marker);
            out.push('\n');
        }
    }

    if shown == 0 {
        out.push_str("(no matching runs)\n");
    }
    out
}

fn render_change(
    out: &mut String,
    change: &ChangeRecord,
    opts: &LogRenderOptions<'_>,
    breaking_marker: &str,
) {
    let name_pad = format!("{:<width$}", change.name, width = PLUGIN_NAME_PAD);
    match &change.from {
        None => {
            out.push_str(&format!(
                "  {} (new install) \u{2192} {}\n",
                name_pad,
                short_hash(&change.to)
            ));
        }
        Some(from) => {
            let n = change.subjects.len();
            out.push_str(&format!(
                "  {} {}..{}  ({} commit{})\n",
                name_pad,
                short_hash(from),
                short_hash(&change.to),
                n,
                if n == 1 { "" } else { "s" }
            ));
            for subj in &change.subjects {
                if change.breaking_subjects.iter().any(|b| b == subj) {
                    out.push_str(&format!("    {} {}\n", breaking_marker, subj));
                } else {
                    out.push_str(&format!("    {}\n", subj));
                }
            }
            if !change.doc_files_changed.is_empty() {
                if opts.diff {
                    for f in &change.doc_files_changed {
                        out.push_str(&format!("    \u{2500}\u{2500} diff: {}\n", f));
                        let key = DiffKey {
                            url: change.url.clone(),
                            from: from.clone(),
                            to: change.to.clone(),
                            file: f.clone(),
                        };
                        if let Some(patch) = opts.diffs.get(&key) {
                            for line in patch.lines() {
                                out.push_str("    ");
                                out.push_str(line);
                                out.push('\n');
                            }
                        }
                    }
                } else {
                    out.push_str(&format!(
                        "    docs changed: {}  (use --diff to view)\n",
                        change.doc_files_changed.join(", ")
                    ));
                }
            }
        }
    }
}

/// アイコンスタイルに応じて BREAKING の prefix を返す。
/// - Nerd / Unicode → `"\u{26a0} BREAKING"` (黄色 ANSI)
/// - Ascii → `"BREAKING"` (装飾なし)
pub fn breaking_marker_for(icons: IconStyle) -> &'static str {
    match icons {
        IconStyle::Nerd | IconStyle::Unicode => "\x1b[33m\u{26a0} BREAKING\x1b[0m",
        IconStyle::Ascii => "BREAKING",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    // ================================================================
    // is_breaking
    // ================================================================

    #[test]
    fn test_is_breaking_subject_bang_simple() {
        assert!(is_breaking("feat!: rewrite api", ""));
    }

    #[test]
    fn test_is_breaking_subject_bang_with_scope() {
        assert!(is_breaking("fix(parser)!: drop legacy field", ""));
    }

    #[test]
    fn test_is_breaking_subject_no_bang_is_not_breaking() {
        assert!(!is_breaking("feat: add new flag", ""));
        assert!(!is_breaking("fix(parser): handle empty", ""));
    }

    #[test]
    fn test_is_breaking_subject_uppercase_type() {
        assert!(is_breaking("Feat!: redo API", ""));
    }

    #[test]
    fn test_is_breaking_subject_empty_scope_rejected() {
        assert!(!is_breaking("feat()!: bad", ""));
    }

    #[test]
    fn test_is_breaking_body_breaking_change() {
        let body = "Some body text\n\nBREAKING CHANGE: removes API\n";
        assert!(is_breaking("fix: small fix", body));
    }

    #[test]
    fn test_is_breaking_body_breaking_dash_change() {
        let body = "BREAKING-CHANGE: gone\n";
        assert!(is_breaking("docs: x", body));
    }

    #[test]
    fn test_is_breaking_body_case_insensitive() {
        let body = "breaking change: lowercase form\n";
        assert!(is_breaking("docs: x", body));
    }

    #[test]
    fn test_is_breaking_body_indented_line_still_counts() {
        // git-format-patch などで先頭スペース入る場合も拾う
        let body = "    BREAKING CHANGE: indented\n";
        assert!(is_breaking("fix: x", body));
    }

    #[test]
    fn test_is_breaking_body_no_marker() {
        assert!(!is_breaking(
            "fix: ok",
            "Just a regular body talking about breaking things abstractly\n"
        ));
    }

    #[test]
    fn test_is_breaking_subject_only_alpha_no_colon_rejected() {
        // `feat!` 単独は subject ではない
        assert!(!is_breaking("feat! whatever", ""));
    }

    // ================================================================
    // serde round-trip
    // ================================================================

    #[test]
    fn test_serde_roundtrip_minimal() {
        let log = UpdateLog {
            runs: vec![RunRecord {
                timestamp: "2026-01-02T03:04:05Z".into(),
                command: "sync".into(),
                changes: vec![ChangeRecord {
                    name: "snacks.nvim".into(),
                    url: "folke/snacks.nvim".into(),
                    from: Some("a".repeat(40)),
                    to: "b".repeat(40),
                    subjects: vec!["fix: x".into()],
                    breaking_subjects: vec![],
                    doc_files_changed: vec!["README.md".into()],
                }],
            }],
        };
        let json = serde_json::to_string(&log).unwrap();
        let back: UpdateLog = serde_json::from_str(&json).unwrap();
        assert_eq!(log, back);
    }

    #[test]
    fn test_serde_roundtrip_fresh_clone() {
        let log = UpdateLog {
            runs: vec![RunRecord {
                timestamp: "2026-04-19T00:00:00Z".into(),
                command: "add".into(),
                changes: vec![ChangeRecord {
                    name: "flash.nvim".into(),
                    url: "folke/flash.nvim".into(),
                    from: None,
                    to: "c".repeat(40),
                    subjects: vec![],
                    breaking_subjects: vec![],
                    doc_files_changed: vec![],
                }],
            }],
        };
        let json = serde_json::to_string_pretty(&log).unwrap();
        let back: UpdateLog = serde_json::from_str(&json).unwrap();
        assert_eq!(log, back);
        assert!(back.runs[0].changes[0].from.is_none());
    }

    #[test]
    fn test_load_log_missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.json");
        let log = load_log(&path);
        assert_eq!(log, UpdateLog::default());
    }

    #[test]
    fn test_load_log_malformed_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"{not valid json").unwrap();
        let log = load_log(&path);
        assert_eq!(log, UpdateLog::default());
    }

    // ================================================================
    // cap_runs
    // ================================================================

    #[test]
    fn test_cap_runs_truncates_oldest() {
        let mut log = UpdateLog::default();
        for i in 0..(MAX_RUNS + 5) {
            log.runs.push(RunRecord {
                timestamp: format!("2026-01-{:02}T00:00:00Z", (i % 28) + 1),
                command: "sync".into(),
                changes: vec![],
            });
        }
        cap_runs(&mut log);
        assert_eq!(log.runs.len(), MAX_RUNS);
        // 最初の 5 件 (古い) が落ち、添字 5..25 の中身が残る
        assert!(
            log.runs
                .first()
                .unwrap()
                .timestamp
                .starts_with("2026-01-06"),
            "got {:?}",
            log.runs.first().map(|r| &r.timestamp)
        );
    }

    #[test]
    fn test_cap_runs_no_op_when_under_limit() {
        let mut log = UpdateLog {
            runs: vec![RunRecord {
                timestamp: "2026-01-01T00:00:00Z".into(),
                command: "sync".into(),
                changes: vec![],
            }],
        };
        cap_runs(&mut log);
        assert_eq!(log.runs.len(), 1);
    }

    fn sample_change(name: &str) -> ChangeRecord {
        ChangeRecord {
            name: name.to_string(),
            url: format!("owner/{}", name),
            from: Some("a".into()),
            to: "b".into(),
            subjects: vec!["fix: x".into()],
            breaking_subjects: vec![],
            doc_files_changed: vec![],
        }
    }

    #[test]
    fn test_record_run_creates_file_and_persists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("update_log.json");
        record_run(&path, "sync", vec![sample_change("a")]).unwrap();
        assert!(path.exists());
        let log = load_log(&path);
        assert_eq!(log.runs.len(), 1);
        assert_eq!(log.runs[0].command, "sync");
    }

    #[test]
    fn test_record_run_skips_empty_changes() {
        // 空 run を persist すると MAX_RUNS cap で有用履歴を押し出すので書かない。
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("update_log.json");
        record_run(&path, "sync", vec![]).unwrap();
        assert!(
            !path.exists(),
            "update_log.json should not be created for empty run"
        );
    }

    #[test]
    fn test_record_run_skips_empty_but_preserves_existing() {
        // 既存 file があっても空 run は追加しない (既存履歴を守る)。
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("update_log.json");
        record_run(&path, "sync", vec![sample_change("a")]).unwrap();
        record_run(&path, "sync", vec![]).unwrap();
        let log = load_log(&path);
        assert_eq!(log.runs.len(), 1, "empty run should not append");
    }

    #[test]
    fn test_record_run_appends_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("update_log.json");
        record_run(&path, "sync", vec![sample_change("a")]).unwrap();
        record_run(&path, "update", vec![sample_change("b")]).unwrap();
        let log = load_log(&path);
        assert_eq!(log.runs.len(), 2);
        assert_eq!(log.runs[0].command, "sync");
        assert_eq!(log.runs[1].command, "update");
    }

    #[test]
    fn test_record_run_caps_at_max_runs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("update_log.json");
        for i in 0..(MAX_RUNS + 3) {
            record_run(&path, "sync", vec![sample_change(&format!("p{}", i))]).unwrap();
        }
        let log = load_log(&path);
        assert_eq!(log.runs.len(), MAX_RUNS);
    }

    // ================================================================
    // RFC3339 / civil-from-days
    // ================================================================

    #[test]
    fn test_format_rfc3339_utc_unix_epoch() {
        let s = format_rfc3339_utc(UNIX_EPOCH);
        assert_eq!(s, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn test_format_rfc3339_utc_known_date() {
        // 2026-04-19T12:34:56Z
        let secs = unix_secs_from_civil(2026, 4, 19, 12, 34, 56).unwrap();
        let t = UNIX_EPOCH + Duration::from_secs(secs as u64);
        assert_eq!(format_rfc3339_utc(t), "2026-04-19T12:34:56Z");
    }

    #[test]
    fn test_parse_rfc3339_round_trip() {
        let original = "2024-02-29T23:59:59Z";
        let t = parse_rfc3339_utc(original).unwrap();
        assert_eq!(format_rfc3339_utc(t), original);
    }

    #[test]
    fn test_parse_rfc3339_accepts_plus_zero() {
        let a = parse_rfc3339_utc("2026-04-19T00:00:00Z").unwrap();
        let b = parse_rfc3339_utc("2026-04-19T00:00:00+00:00").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn test_parse_rfc3339_rejects_garbage() {
        assert!(parse_rfc3339_utc("not a date").is_none());
        assert!(parse_rfc3339_utc("2026-13-40T99:99:99Z").is_none());
    }

    // ================================================================
    // format_relative
    // ================================================================

    #[test]
    fn test_format_relative_just_now() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let then = now - Duration::from_secs(30);
        assert_eq!(format_relative(then, now), "Just now");
    }

    #[test]
    fn test_format_relative_minutes() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let then = now - Duration::from_secs(5 * 60);
        assert_eq!(format_relative(then, now), "5 minutes ago");
        let then1 = now - Duration::from_secs(60);
        assert_eq!(format_relative(then1, now), "1 minute ago");
    }

    #[test]
    fn test_format_relative_hours() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let then = now - Duration::from_secs(3 * 3600);
        assert_eq!(format_relative(then, now), "3 hours ago");
    }

    #[test]
    fn test_format_relative_days() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let then = now - Duration::from_secs(2 * 86_400);
        assert_eq!(format_relative(then, now), "2 days ago");
    }

    #[test]
    fn test_format_relative_absolute_after_week() {
        // 30 日前 → 絶対日付フォーマット
        let now_secs = unix_secs_from_civil(2026, 4, 19, 12, 0, 0).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(now_secs as u64);
        let then = now - Duration::from_secs(30 * 86_400);
        let rendered = format_relative(then, now);
        assert!(
            rendered.starts_with("2026-03-"),
            "expected 2026-03-XX, got {}",
            rendered
        );
    }

    // ================================================================
    // render_log
    // ================================================================

    fn sample_log() -> UpdateLog {
        UpdateLog {
            runs: vec![RunRecord {
                timestamp: "2026-04-19T00:00:00Z".into(),
                command: "update".into(),
                changes: vec![
                    ChangeRecord {
                        name: "snacks.nvim".into(),
                        url: "folke/snacks.nvim".into(),
                        from: Some("abc1234aaaa".into()),
                        to: "def5678bbbb".into(),
                        subjects: vec![
                            "feat!: rewrite picker API".into(),
                            "fix: handle empty buffer".into(),
                        ],
                        breaking_subjects: vec!["feat!: rewrite picker API".into()],
                        doc_files_changed: vec!["README.md".into(), "doc/snacks.txt".into()],
                    },
                    ChangeRecord {
                        name: "flash.nvim".into(),
                        url: "folke/flash.nvim".into(),
                        from: None,
                        to: "999aaaa".into(),
                        subjects: vec![],
                        breaking_subjects: vec![],
                        doc_files_changed: vec![],
                    },
                ],
            }],
        }
    }

    fn render_with(opts: LogRenderOptions<'_>) -> String {
        render_log(&sample_log(), &opts)
    }

    fn now_for_log() -> SystemTime {
        // sample timestamp + 10 minutes → "10 minutes ago"
        UNIX_EPOCH
            + Duration::from_secs(unix_secs_from_civil(2026, 4, 19, 0, 10, 0).unwrap() as u64)
    }

    #[test]
    fn test_render_log_basic_no_query() {
        let s = render_with(LogRenderOptions {
            last: 1,
            query: None,
            full: false,
            diff: false,
            diffs: HashMap::new(),
            icons: IconStyle::Ascii,
            now: now_for_log(),
        });
        assert!(s.contains("10 minutes ago"), "got:\n{}", s);
        assert!(s.contains("update (2 plugins)"), "got:\n{}", s);
        assert!(s.contains("snacks.nvim"), "got:\n{}", s);
        assert!(s.contains("abc1234..def5678"), "got:\n{}", s);
        assert!(s.contains("(2 commits)"), "got:\n{}", s);
        // ASCII style → "BREAKING" prefix without escape codes
        assert!(s.contains("BREAKING feat!: rewrite picker API"));
        // fresh install
        assert!(s.contains("(new install)"));
        assert!(s.contains("999aaaa"));
        // docs hint when --diff is off
        assert!(s.contains("docs changed: README.md, doc/snacks.txt"));
        assert!(s.contains("(use --diff to view)"));
    }

    #[test]
    fn test_render_log_query_filters_plugins() {
        let s = render_with(LogRenderOptions {
            last: 1,
            query: Some("flash"),
            full: false,
            diff: false,
            diffs: HashMap::new(),
            icons: IconStyle::Ascii,
            now: now_for_log(),
        });
        assert!(s.contains("flash.nvim"), "got:\n{}", s);
        assert!(!s.contains("snacks.nvim"), "got:\n{}", s);
        // Only 1 plugin matched → header should reflect that
        assert!(s.contains("(1 plugin)"), "got:\n{}", s);
    }

    #[test]
    fn test_render_log_query_no_match_omits_run() {
        let s = render_with(LogRenderOptions {
            last: 1,
            query: Some("definitely-nope"),
            full: false,
            diff: false,
            diffs: HashMap::new(),
            icons: IconStyle::Ascii,
            now: now_for_log(),
        });
        assert!(s.contains("(no matching runs)"), "got:\n{}", s);
    }

    #[test]
    fn test_render_log_diff_embeds_patch() {
        let mut diffs = HashMap::new();
        diffs.insert(
            DiffKey {
                url: "folke/snacks.nvim".into(),
                from: "abc1234aaaa".into(),
                to: "def5678bbbb".into(),
                file: "README.md".into(),
            },
            "diff --git a/README.md b/README.md\n+ added line\n".to_string(),
        );
        let s = render_with(LogRenderOptions {
            last: 1,
            query: None,
            full: false,
            diff: true,
            diffs,
            icons: IconStyle::Ascii,
            now: now_for_log(),
        });
        assert!(s.contains("diff: README.md"), "got:\n{}", s);
        assert!(s.contains("+ added line"), "got:\n{}", s);
        // Without --diff the hint shouldn't appear
        assert!(!s.contains("(use --diff to view)"));
    }

    #[test]
    fn test_render_log_empty_log_message() {
        let log = UpdateLog::default();
        let opts = LogRenderOptions {
            last: 1,
            query: None,
            full: false,
            diff: false,
            diffs: HashMap::new(),
            icons: IconStyle::Ascii,
            now: now_for_log(),
        };
        let s = render_log(&log, &opts);
        assert!(s.contains("(no runs recorded yet)"));
    }

    #[test]
    fn test_render_log_breaking_marker_unicode_has_warn() {
        let s = render_with(LogRenderOptions {
            last: 1,
            query: None,
            full: false,
            diff: false,
            diffs: HashMap::new(),
            icons: IconStyle::Unicode,
            now: now_for_log(),
        });
        // Unicode/Nerd → ANSI yellow + ⚠ prefix
        assert!(
            s.contains("\u{26a0} BREAKING"),
            "expected warn icon in BREAKING marker, got:\n{}",
            s
        );
    }

    // ================================================================
    // short_hash
    // ================================================================

    #[test]
    fn test_short_hash_truncates_to_7() {
        assert_eq!(short_hash("0123456789abcdef"), "0123456");
    }

    #[test]
    fn test_short_hash_handles_short_input() {
        assert_eq!(short_hash("abc"), "abc");
        assert_eq!(short_hash(""), "");
    }
}
