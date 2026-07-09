//! `<cache_root>/update_errors.json` の read/write。
//!
//! 背景:
//! - `rvpm update` が失敗しても、clone 自体は健全なまま (古い commit に留まる)
//!   なので `rvpm list` の git status は `Clean` を返し、`[Clean]` と表示される。
//!   結果「update がコケたのにどこにも痕跡が残らない」罠になっていた。
//! - そこで **plugin 単位で「最後の update が失敗した」事実を記録** して、
//!   `rvpm list` がそれを overlay 表示する。成功した `update` / `sync` で
//!   該当エントリを消し込む (stale なマーカーを残さない)。
//!
//! スキーマ:
//! ```json
//! { "version": 1,
//!   "entries": [
//!     { "name": "snacks.nvim", "url": "folke/snacks.nvim",
//!       "message": "failed to fetch: ...", "timestamp": "2026-07-09T12:34:56Z" }
//!   ] }
//! ```
//!
//! - **場所**: `<cache_root>/update_errors.json` (ephemeral cache 側。fetch_state /
//!   cooldown_state と同じ場所・同じ流儀)。
//! - **resilience**: malformed / missing → empty state にフォールバック。ユーザー
//!   操作は止めない。
//! - **schema version**: 未対応バージョンは empty 扱い (fetch_state と同じパターン)。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 現行スキーマバージョン。壊すときは bump + migration を足す。
pub const CURRENT_VERSION: u32 = 1;

/// update_errors のルート構造。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateErrors {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<UpdateErrorEntry>,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}

impl Default for UpdateErrors {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            entries: Vec::new(),
        }
    }
}

/// 1 プラグイン分の「最後に失敗した update」エントリ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateErrorEntry {
    /// `Plugin::display_name()` 由来の lookup キー。
    pub name: String,
    /// config.toml に書かれた URL。同じ name で別 repo に差し替えられた時の検知用
    /// (lockfile / fetch_state と同じ思想)。
    pub url: String,
    /// 失敗理由 (git エラーメッセージ等)。
    pub message: String,
    /// 失敗を記録した時刻。RFC3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`)。
    pub timestamp: String,
}

impl UpdateErrors {
    /// `path` から読み出す。
    /// - 存在しない → `Default` (empty)
    /// - パース失敗 / version mismatch → warn を出して `Default`
    pub fn load(path: &Path) -> Self {
        let content = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                eprintln!(
                    "\u{26a0} update_errors: failed to read {}: {} (treating as empty)",
                    path.display(),
                    e
                );
                return Self::default();
            }
        };
        match serde_json::from_str::<UpdateErrors>(&content) {
            Ok(s) if s.version == CURRENT_VERSION => s,
            Ok(s) => {
                eprintln!(
                    "\u{26a0} update_errors: unsupported version {} in {} (expected {}; treating as empty)",
                    s.version,
                    path.display(),
                    CURRENT_VERSION
                );
                Self::default()
            }
            Err(e) => {
                eprintln!(
                    "\u{26a0} update_errors: failed to parse {}: {} (treating as empty)",
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
        let body = serde_json::to_string_pretty(self).context("serialize update_errors")?;
        let parent = path.parent().unwrap_or(Path::new("."));
        let tmp = tempfile::Builder::new()
            .prefix(".rvpm-update-errors-")
            .suffix(".tmp")
            .tempfile_in(parent)
            .with_context(|| format!("create tempfile in {}", parent.display()))?;
        std::fs::write(tmp.path(), body.as_bytes())
            .with_context(|| format!("write tempfile {}", tmp.path().display()))?;
        tmp.persist(path)
            .map_err(|e| anyhow::anyhow!("rename tempfile to {}: {}", path.display(), e))?;
        Ok(())
    }

    /// 単発検索 API。本番の list は事前に `HashMap` lookup を組んで O(N) で回す
    /// ので使っていないが、将来の consumer と tests 用に残す (fetch_state と同じ)。
    #[allow(dead_code)]
    pub fn find(&self, name: &str) -> Option<&UpdateErrorEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn upsert(&mut self, entry: UpdateErrorEntry) {
        if let Some(slot) = self.entries.iter_mut().find(|e| e.name == entry.name) {
            *slot = entry;
        } else {
            self.entries.push(entry);
        }
    }

    /// `name` のエントリを消し込む (update / sync が成功したとき)。エントリが
    /// あって実際に削除したら `true`、無ければ `false` を返す (呼び出し側が
    /// dirty 判定に使えるように)。
    pub fn remove_by_name(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.name != name);
        self.entries.len() != before
    }

    /// `names` に無い entry を drop する (config.toml から外されたプラグイン)。
    pub fn retain_by_names(&mut self, names: &std::collections::HashSet<String>) {
        self.entries.retain(|e| names.contains(&e.name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn mk(name: &str, message: &str) -> UpdateErrorEntry {
        UpdateErrorEntry {
            name: name.to_string(),
            url: format!("owner/{}", name),
            message: message.to_string(),
            timestamp: "2026-07-09T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn test_load_missing_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let state = UpdateErrors::load(&path);
        assert_eq!(state.version, CURRENT_VERSION);
        assert!(state.entries.is_empty());
    }

    #[test]
    fn test_load_malformed_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not valid json =====").unwrap();
        let state = UpdateErrors::load(&path);
        assert!(state.entries.is_empty());
    }

    #[test]
    fn test_save_then_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("update_errors.json");
        let mut state = UpdateErrors::default();
        state.upsert(mk("a", "boom"));
        state.upsert(mk("b", "network down"));
        state.save(&path).unwrap();

        let loaded = UpdateErrors::load(&path);
        assert_eq!(loaded.version, CURRENT_VERSION);
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.find("a").map(|e| e.message.as_str()), Some("boom"));
        assert_eq!(
            loaded.find("b").map(|e| e.message.as_str()),
            Some("network down")
        );
    }

    #[test]
    fn test_save_sorts_entries_by_name_for_stable_diffs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("update_errors.json");
        let mut state = UpdateErrors::default();
        state.upsert(mk("zeta", "z"));
        state.upsert(mk("alpha", "a"));
        state.upsert(mk("mid", "m"));
        state.save(&path).unwrap();

        let loaded = UpdateErrors::load(&path);
        let names: Vec<_> = loaded.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn test_upsert_replaces_existing_entry() {
        let mut state = UpdateErrors::default();
        state.upsert(mk("a", "old"));
        state.upsert(mk("a", "new"));
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].message, "new");
    }

    #[test]
    fn test_remove_by_name_clears_and_reports() {
        let mut state = UpdateErrors::default();
        state.upsert(mk("a", "boom"));
        state.upsert(mk("b", "boom"));
        assert!(state.remove_by_name("a"));
        assert!(state.find("a").is_none());
        assert_eq!(state.entries.len(), 1);
        // 存在しない name の削除は false
        assert!(!state.remove_by_name("missing"));
    }

    #[test]
    fn test_retain_by_names_drops_orphans() {
        let mut state = UpdateErrors::default();
        state.upsert(mk("a", "x"));
        state.upsert(mk("b", "x"));
        state.upsert(mk("c", "x"));
        let mut keep = std::collections::HashSet::new();
        keep.insert("a".to_string());
        keep.insert("c".to_string());
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
            r#"{"version":99,"entries":[{"name":"x","url":"o/x","message":"m","timestamp":"2026-07-09T00:00:00Z"}]}"#,
        )
        .unwrap();
        let state = UpdateErrors::load(&path);
        assert!(
            state.entries.is_empty(),
            "future schema must degrade to empty state"
        );
        assert_eq!(state.version, CURRENT_VERSION);
    }
}
