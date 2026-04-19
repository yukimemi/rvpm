//! `<cache_root>/merge_conflicts.json` の read/write ヘルパー。
//!
//! `run_sync` / `run_generate` は merged/ を構築する過程で first-wins の
//! 衝突を収集する。この情報は stderr に print するだけでは消えてしまい、
//! `rvpm doctor` からも見えないので、毎回この JSON に**上書き保存**する。
//! doctor は読み出して warn を出す。
//!
//! 設計:
//! - 常に最新の 1 回分のみ保存 (履歴は持たない。`update_log.json` と違い
//!   conflict は**永続的な状態**なので次回 sync まで有効な snapshot として扱う)。
//! - sync 完了時に conflict が 0 件でも書き込む (古い snapshot が doctor に
//!   誤判定を出させないよう、最新 sync の clean 状態を反映させる)。
//! - 書き込みは `update_log.rs::save_log` と同じ tempfile + rename パターン。
//!
//! スキーマ:
//! ```json
//! {
//!   "timestamp": "2026-04-19T07:30:00Z",
//!   "reports": [
//!     { "loser": "smart-splits.nvim", "winner": "nvim-tree.lua",
//!       "relative": "plugin/init.lua" }
//!   ]
//! }
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::SystemTime;

use crate::update_log::format_rfc3339_utc;

/// 1 衝突分のレポート。merge_conflicts.json に永続化される個々のエントリ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConflictReport {
    /// 衝突で skip された plugin の表示名。
    pub loser: String,
    /// 先に同じ path を置いた plugin の表示名。特定できないケース
    /// (merged/ に既に別経路で存在した等) は None。
    #[serde(default)]
    pub winner: Option<String>,
    /// merged/ 相対の衝突 path (forward slash 固定)。例: `"plugin/init.lua"`。
    pub relative: String,
}

/// 1 回の sync/generate で発生した衝突スナップショット。
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConflictSnapshot {
    /// RFC3339 UTC. スナップショット保存時刻。
    pub timestamp: String,
    pub reports: Vec<MergeConflictReport>,
}

/// `<cache_root>/merge_conflicts.json` を読み出す。
/// 存在しない / パース失敗時は空の snapshot を返す (resilience)。
pub fn load_snapshot(path: &Path) -> MergeConflictSnapshot {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return MergeConflictSnapshot::default();
        }
        Err(e) => {
            eprintln!(
                "\u{26a0} merge_conflicts: failed to read {}: {} (treating as empty)",
                path.display(),
                e
            );
            return MergeConflictSnapshot::default();
        }
    };
    serde_json::from_str::<MergeConflictSnapshot>(&content).unwrap_or_else(|e| {
        eprintln!(
            "\u{26a0} merge_conflicts: failed to parse {}: {} (treating as empty)",
            path.display(),
            e
        );
        MergeConflictSnapshot::default()
    })
}

/// 現時刻の timestamp で snapshot を保存する (atomic write)。
/// `reports` が空でも書き込む — 直近の sync で衝突が消えたことを doctor が
/// 把握できるようにするため。
pub fn save_snapshot(path: &Path, reports: Vec<MergeConflictReport>) -> Result<()> {
    let snapshot = MergeConflictSnapshot {
        timestamp: format_rfc3339_utc(SystemTime::now()),
        reports,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&snapshot).context("serialize merge_conflicts")?;
    let parent = path.parent().unwrap_or(Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix(".rvpm-merge-conflicts-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("create tempfile in {}", parent.display()))?;
    std::fs::write(tmp.path(), json.as_bytes())
        .with_context(|| format!("write tempfile {}", tmp.path().display()))?;
    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("rename tempfile to {}: {}", path.display(), e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_load_snapshot_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let snap = load_snapshot(&path);
        assert!(snap.timestamp.is_empty());
        assert!(snap.reports.is_empty());
    }

    #[test]
    fn test_load_snapshot_malformed_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{not valid json").unwrap();
        let snap = load_snapshot(&path);
        assert!(snap.reports.is_empty());
    }

    #[test]
    fn test_save_then_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("merge_conflicts.json");
        let reports = vec![
            MergeConflictReport {
                loser: "smart-splits.nvim".to_string(),
                winner: Some("nvim-tree.lua".to_string()),
                relative: "plugin/init.lua".to_string(),
            },
            MergeConflictReport {
                loser: "other".to_string(),
                winner: None,
                relative: "lua/x/y.lua".to_string(),
            },
        ];
        save_snapshot(&path, reports.clone()).unwrap();
        let snap = load_snapshot(&path);
        assert_eq!(snap.reports, reports);
        // timestamp は RFC3339 Z 形式
        assert!(snap.timestamp.ends_with('Z'));
        assert!(snap.timestamp.contains('T'));
    }

    #[test]
    fn test_save_empty_reports_still_writes_file() {
        // 衝突 0 件でも書き出す (直近 sync が clean であったことを doctor が
        // 判別できるようにするため)。
        let dir = tempdir().unwrap();
        let path = dir.path().join("merge_conflicts.json");
        save_snapshot(&path, Vec::new()).unwrap();
        assert!(path.exists());
        let snap = load_snapshot(&path);
        assert!(snap.reports.is_empty());
        assert!(!snap.timestamp.is_empty());
    }

    #[test]
    fn test_save_overwrites_previous_snapshot() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("merge_conflicts.json");
        let first = vec![MergeConflictReport {
            loser: "a".to_string(),
            winner: Some("b".to_string()),
            relative: "x".to_string(),
        }];
        save_snapshot(&path, first).unwrap();
        save_snapshot(&path, Vec::new()).unwrap();
        let snap = load_snapshot(&path);
        assert!(snap.reports.is_empty());
    }

    #[test]
    fn test_report_missing_winner_defaults_to_none_on_parse() {
        // 古いフォーマット (winner キーが無い) でも壊れないよう、#[serde(default)]
        // で None に落ちることを担保する。
        let dir = tempdir().unwrap();
        let path = dir.path().join("old.json");
        std::fs::write(
            &path,
            r#"{"timestamp":"2026-04-19T00:00:00Z","reports":[{"loser":"x","relative":"a/b"}]}"#,
        )
        .unwrap();
        let snap = load_snapshot(&path);
        assert_eq!(snap.reports.len(), 1);
        assert!(snap.reports[0].winner.is_none());
    }
}
