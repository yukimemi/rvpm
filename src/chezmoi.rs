//! chezmoi 連携ヘルパー。
//!
//! `options.chezmoi = true` のとき、rvpm は config.toml や per-plugin hook を
//! **chezmoi source 側に直接書き込み**、`chezmoi apply` で target へ反映する。
//! これにより chezmoi の「source が truth」原則に沿った連携が実現できる。
//!
//! 前提: chezmoi = true のとき、管理対象ファイルは **plain file** であること。
//! `.tmpl` (chezmoi テンプレート) は非対応。rvpm 自身が Tera テンプレートを
//! 持っているため chezmoi のテンプレート機能は不要。

use std::path::{Path, PathBuf};
use std::process::Command;

/// target パスに対応する chezmoi source パスを解決する (pure function)。
///
/// 1. `source_path_probe(target)` が Some を返せばそのまま使う
/// 2. 返さなければ (新規ファイル等) 祖先を遡り、最初に managed な祖先から
///    相対パスを計算して source 側のフルパスを構築する
/// 3. source パスが `.tmpl` で終わる場合は None (テンプレート非対応)
///
/// `source_path_probe` は「`chezmoi source-path <p>` の結果」を返すクロージャ。
/// テストでは mock を差し込む。
pub fn resolve_source_path<F>(target: &Path, mut source_path_probe: F) -> Option<PathBuf>
where
    F: FnMut(&Path) -> Option<PathBuf>,
{
    // target 自体が managed なケース (既存ファイル)
    if let Some(sp) = source_path_probe(target) {
        if is_tmpl(&sp) {
            eprintln!(
                "\u{26a0} {} is a chezmoi template (.tmpl). \
                 chezmoi=true requires plain files — use rvpm's Tera templates instead.",
                target.display(),
            );
            return None;
        }
        return Some(sp);
    }
    // 新規ファイル: 祖先を遡って managed な ancestor を見つけ、相対パスで join
    let mut ancestor = target.parent();
    while let Some(a) = ancestor {
        if let Some(source_ancestor) = source_path_probe(a) {
            if is_tmpl(&source_ancestor) {
                return None;
            }
            let relative = target.strip_prefix(a).ok()?;
            return Some(source_ancestor.join(relative));
        }
        ancestor = a.parent();
    }
    None
}

fn is_tmpl(p: &Path) -> bool {
    p.to_string_lossy().ends_with(".tmpl")
}

/// `chezmoi source-path <target>` を実行し、managed なら source パスを返す。
fn chezmoi_source_path(target: &Path) -> Option<PathBuf> {
    let output = match Command::new("chezmoi")
        .arg("source-path")
        .arg(target)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "\u{26a0} chezmoi source-path {} failed: {}",
                target.display(),
                e,
            );
            return None;
        }
    };
    if output.status.success() {
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    } else {
        None
    }
}

fn is_chezmoi_available() -> bool {
    Command::new("chezmoi")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// rvpm が書き込むべきパスを返す。chezmoi 有効かつ managed なら source 側、
/// そうでなければ target そのまま。
pub fn write_path(enabled: bool, target: &Path) -> PathBuf {
    if !enabled {
        return target.to_path_buf();
    }
    if !is_chezmoi_available() {
        eprintln!(
            "\u{26a0} options.chezmoi = true but `chezmoi` is not in PATH. \
             Writing to target directly (install chezmoi or set chezmoi = false).",
        );
        return target.to_path_buf();
    }
    resolve_source_path(target, chezmoi_source_path).unwrap_or_else(|| target.to_path_buf())
}

/// source に書いた後、`chezmoi apply <target>` で target へ反映する。
pub fn apply(enabled: bool, target: &Path) {
    if !enabled {
        return;
    }
    if !is_chezmoi_available() {
        return;
    }
    // target が managed じゃなければ apply は不要 (write_path が target を返した場合)
    if chezmoi_source_path(target).is_none() {
        // 祖先が managed なら新規ファイルとして apply が必要
        let has_managed_ancestor = target
            .ancestors()
            .skip(1)
            .any(|a| chezmoi_source_path(a).is_some());
        if !has_managed_ancestor {
            return;
        }
    }
    let mut cmd = Command::new("chezmoi");
    cmd.arg("apply").arg(target);
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!(
            "\u{26a0} chezmoi apply {} failed (exit {})",
            target.display(),
            s.code().unwrap_or(-1),
        ),
        Err(e) => eprintln!("\u{26a0} chezmoi apply {} failed: {}", target.display(), e,),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn mock_probe(managed: HashMap<PathBuf, PathBuf>) -> impl FnMut(&Path) -> Option<PathBuf> {
        move |p| managed.get(p).cloned()
    }

    #[test]
    fn test_resolve_existing_managed_file() {
        let target = PathBuf::from("/home/user/.config/rvpm/nvim/config.toml");
        let source =
            PathBuf::from("/home/user/.local/share/chezmoi/dot_config/rvpm/nvim/config.toml");
        let managed = HashMap::from([(target.clone(), source.clone())]);
        let got = resolve_source_path(&target, mock_probe(managed));
        assert_eq!(got, Some(source));
    }

    #[test]
    fn test_resolve_new_file_via_managed_ancestor() {
        let ancestor_target = PathBuf::from("/home/user/.config/rvpm/nvim/plugins");
        let ancestor_source =
            PathBuf::from("/home/user/.local/share/chezmoi/dot_config/rvpm/nvim/plugins");
        let target = ancestor_target.join("github.com/foo/bar/init.lua");
        let managed = HashMap::from([(ancestor_target, ancestor_source.clone())]);
        let got = resolve_source_path(&target, mock_probe(managed));
        assert_eq!(
            got,
            Some(ancestor_source.join("github.com/foo/bar/init.lua"))
        );
    }

    #[test]
    fn test_resolve_tmpl_returns_none() {
        let target = PathBuf::from("/home/user/.config/rvpm/nvim/config.toml");
        let source =
            PathBuf::from("/home/user/.local/share/chezmoi/dot_config/rvpm/nvim/config.toml.tmpl");
        let managed = HashMap::from([(target.clone(), source)]);
        let got = resolve_source_path(&target, mock_probe(managed));
        assert_eq!(got, None);
    }

    #[test]
    fn test_resolve_not_managed_returns_none() {
        let target = PathBuf::from("/home/user/.config/rvpm/nvim/config.toml");
        let managed = HashMap::new();
        let got = resolve_source_path(&target, mock_probe(managed));
        assert_eq!(got, None);
    }

    #[test]
    fn test_resolve_new_file_no_managed_ancestor_returns_none() {
        let target = PathBuf::from("/home/user/.config/rvpm/nvim/plugins/foo/init.lua");
        let managed = HashMap::new();
        let got = resolve_source_path(&target, mock_probe(managed));
        assert_eq!(got, None);
    }

    #[test]
    fn test_write_path_disabled_returns_target() {
        let target = Path::new("/some/target");
        let got = write_path(false, target);
        assert_eq!(got, target);
    }
}
