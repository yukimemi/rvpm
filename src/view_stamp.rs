//! view / merged の incremental rebuild 用 stamp (#perf)。
//!
//! `views/<plug>/` は plugin clone の hard-link tree なので、 **clone の HEAD
//! commit が変わらない限り中身は再構築不要** (hard link は inode 共有なので
//! ファイル内容の変化は自動追従し、 変わり得るのは「ファイル集合」だけ = commit
//! が動いた時のみ)。 そこで build 完了時に fingerprint を `.rvpm-stamp.json`
//! として view dir 直下に書き込み、 次回 build 時に一致すれば walk + link を
//! 丸ごと skip する。
//!
//! - stamp は `atomic_replace_view_dir` の tmp dir に書かれてから atomic rename
//!   されるので、 「stamp が存在する ⟺ その fingerprint で build が完走した」
//!   が常に成り立つ (中途半端な状態に stamp は残らない)。
//! - 壊れた stamp / 旧 schema / 旧 rvpm version は不一致扱い → full rebuild
//!   (resilience: 判定に失敗しても安全側に倒れるだけ)。
//! - dev plugin は commit と無関係に中身が変わるので stamp 対象外 (caller が
//!   expected=None を渡して常に rebuild する)。
//!
//! `merged/` 用には全寄与 plugin の fingerprint を結合した stamp を同名で置く
//! (`merged_fingerprint`)。

use serde::{Deserialize, Serialize};
use std::path::Path;

/// view dir / merged dir 直下に置く stamp ファイル名。
/// dotfile なので link::walk の merge 対象とは衝突しない (walk は隠しエントリを
/// 全階層 skip する)。 rtp 上にあっても Neovim は感知しない。
pub const STAMP_FILE: &str = ".rvpm-stamp.json";

/// stamp の互換 schema version。 stamp の意味論 (skip 判定に影響する要素) を
/// 変えたら increment して旧 stamp を全部 invalidate する。
pub const STAMP_SCHEMA: u32 = 1;

/// 1 つの view (または merged) の rebuild 要否を判定する fingerprint。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewStamp {
    /// stamp 自体の schema version (`STAMP_SCHEMA`)。
    pub schema: u32,
    /// stamp を書いた rvpm の version。 link / merge ロジックが変わった
    /// リリースを跨いだら安全側で full rebuild する。
    pub rvpm_version: String,
    /// fingerprint 本体。 view なら clone の HEAD commit、 merged なら
    /// 寄与 plugin 全員の (name, commit, mode) を結合した文字列。
    pub fingerprint: String,
}

impl ViewStamp {
    /// 現 rvpm version + 現 schema で fingerprint を包む。
    pub fn new(fingerprint: String) -> Self {
        Self {
            schema: STAMP_SCHEMA,
            rvpm_version: env!("CARGO_PKG_VERSION").to_string(),
            fingerprint,
        }
    }
}

/// `dir/.rvpm-stamp.json` を読む。 無い / 壊れている / 読めない場合は `None`
/// (= 不一致扱いで rebuild)。
pub fn read(dir: &Path) -> Option<ViewStamp> {
    let raw = std::fs::read_to_string(dir.join(STAMP_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// `dir/.rvpm-stamp.json` に stamp を書く。 dir は存在している前提
/// (atomic build の tmp dir に書く想定)。
pub fn write(dir: &Path, stamp: &ViewStamp) -> anyhow::Result<()> {
    let raw = serde_json::to_string(stamp)?;
    std::fs::write(dir.join(STAMP_FILE), raw)?;
    Ok(())
}

/// 「dir が存在し、 stamp が expected と一致するか」= rebuild skip 可能か。
pub fn is_current(dir: &Path, expected: &ViewStamp) -> bool {
    if !dir.is_dir() {
        return false;
    }
    read(dir).as_ref() == Some(expected)
}

/// merged/ 用 fingerprint: 寄与 plugin の (name, commit, 寄与種別) を
/// **caller が渡した順** (= config の sort_plugins 順) で結合する。
/// first-wins の勝敗は処理順に依存するので、 順序も fingerprint の一部。
/// commit が取れなかった plugin (`None`) は毎回ユニークではなく "?" を入れる
/// — その場合 caller 側で skip 自体を諦める判断をするので、 ここでは
/// 文字列化だけ単純に行う。
pub fn merged_fingerprint(parts: &[(String, String, &'static str)]) -> String {
    let mut s = String::new();
    for (name, commit, kind) in parts {
        s.push_str(name);
        s.push('=');
        s.push_str(commit);
        s.push(':');
        s.push_str(kind);
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let stamp = ViewStamp::new("abc123".into());
        write(tmp.path(), &stamp).unwrap();
        assert_eq!(read(tmp.path()), Some(stamp));
    }

    #[test]
    fn test_read_missing_returns_none() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(read(tmp.path()), None);
    }

    #[test]
    fn test_read_corrupt_returns_none() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(STAMP_FILE), "{not json").unwrap();
        assert_eq!(read(tmp.path()), None);
    }

    #[test]
    fn test_is_current_matches() {
        let tmp = TempDir::new().unwrap();
        let stamp = ViewStamp::new("abc123".into());
        write(tmp.path(), &stamp).unwrap();
        assert!(is_current(tmp.path(), &stamp));
    }

    #[test]
    fn test_is_current_fingerprint_mismatch() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), &ViewStamp::new("old".into())).unwrap();
        assert!(!is_current(tmp.path(), &ViewStamp::new("new".into())));
    }

    #[test]
    fn test_is_current_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let gone = tmp.path().join("nope");
        assert!(!is_current(&gone, &ViewStamp::new("x".into())));
    }

    #[test]
    fn test_is_current_schema_bump_invalidates() {
        let tmp = TempDir::new().unwrap();
        let mut old = ViewStamp::new("abc".into());
        old.schema = STAMP_SCHEMA - 1;
        write(tmp.path(), &old).unwrap();
        assert!(!is_current(tmp.path(), &ViewStamp::new("abc".into())));
    }

    #[test]
    fn test_is_current_version_change_invalidates() {
        let tmp = TempDir::new().unwrap();
        let mut old = ViewStamp::new("abc".into());
        old.rvpm_version = "0.0.0-other".into();
        write(tmp.path(), &old).unwrap();
        assert!(!is_current(tmp.path(), &ViewStamp::new("abc".into())));
    }

    #[test]
    fn test_merged_fingerprint_order_sensitive() {
        let a = merged_fingerprint(&[
            ("p1".into(), "c1".into(), "full"),
            ("p2".into(), "c2".into(), "doc"),
        ]);
        let b = merged_fingerprint(&[
            ("p2".into(), "c2".into(), "doc"),
            ("p1".into(), "c1".into(), "full"),
        ]);
        assert_ne!(a, b);
    }
}
