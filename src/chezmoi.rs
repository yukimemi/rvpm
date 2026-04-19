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
use std::time::Duration;

/// 外部 `chezmoi` コマンドに許す最大実行時間。これを越えたら諦めて何もしない
/// (resilience)。`run_doctor` の `VERSION_PROBE_TIMEOUT` と同じ思想で、PATH 上の
/// 壊れた shim や応答しない subprocess で rvpm 全体が hang するのを防ぐ。
const CHEZMOI_TIMEOUT: Duration = Duration::from_secs(2);

/// target パスに対応する chezmoi source パスを解決する純粋ロジック。
///
/// 1. `source_path_probe(target)` が Some を返せばそのまま使う
/// 2. 返さなければ (新規ファイル等) 祖先を遡り、最初に managed な祖先から
///    相対パスを計算して source 側のフルパスを構築する
/// 3. source パスが `.tmpl` で終わる場合は None (テンプレート非対応)
///
/// 本番コードは `resolve_source_path_async` 経由で `chezmoi source-path` を
/// 呼ぶ (2 秒タイムアウト付き)。この sync 版は `source_path_probe` クロージャに
/// mock を差し込めるためテストロジック検証専用に残してある。
#[cfg(test)]
fn resolve_source_path<F>(target: &Path, mut source_path_probe: F) -> Option<PathBuf>
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
                eprintln!(
                    "\u{26a0} ancestor {} resolves to a chezmoi template (.tmpl). \
                     chezmoi=true requires plain files — use rvpm's Tera templates instead.",
                    a.display(),
                );
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
/// 2 秒のタイムアウト付き — chezmoi が hang しても呼び出し側を巻き込まない。
async fn chezmoi_source_path(target: &Path) -> Option<PathBuf> {
    let fut = tokio::process::Command::new("chezmoi")
        .arg("source-path")
        .arg(target)
        .output();
    let output = match tokio::time::timeout(CHEZMOI_TIMEOUT, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            eprintln!(
                "\u{26a0} chezmoi source-path {} failed: {}",
                target.display(),
                e,
            );
            return None;
        }
        Err(_) => {
            eprintln!(
                "\u{26a0} chezmoi source-path {} timed out after {}s",
                target.display(),
                CHEZMOI_TIMEOUT.as_secs(),
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

async fn is_chezmoi_available() -> bool {
    let fut = tokio::process::Command::new("chezmoi")
        .arg("--version")
        .output();
    matches!(
        tokio::time::timeout(CHEZMOI_TIMEOUT, fut).await,
        Ok(Ok(o)) if o.status.success(),
    )
}

/// rvpm が書き込むべきパスを返す。chezmoi 有効かつ managed なら source 側、
/// そうでなければ target そのまま。返り値が target と異なれば source に書いた
/// ことを意味するので、呼び出し側は `apply()` を呼んで target へ反映する。
///
/// 内部で `chezmoi source-path` を target → 各祖先の順に呼ぶため async。各呼び
/// 出しに 2 秒の timeout が付くので、chezmoi が応答しなくても全体で hang しない。
pub async fn write_path(enabled: bool, target: &Path) -> PathBuf {
    if !enabled {
        return target.to_path_buf();
    }
    if !is_chezmoi_available().await {
        eprintln!(
            "\u{26a0} options.chezmoi = true but `chezmoi` is not in PATH. \
             Writing to target directly (install chezmoi or set chezmoi = false).",
        );
        return target.to_path_buf();
    }
    resolve_source_path_async(target)
        .await
        .unwrap_or_else(|| target.to_path_buf())
}

/// `resolve_source_path` の async 版。`chezmoi_source_path` を直接呼ぶため
/// テスト用 mock は差し込めない (純粋なロジック検証は同期版 `resolve_source_path`
/// で担保する)。
async fn resolve_source_path_async(target: &Path) -> Option<PathBuf> {
    if let Some(sp) = chezmoi_source_path(target).await {
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
    let mut ancestor = target.parent();
    while let Some(a) = ancestor {
        if let Some(source_ancestor) = chezmoi_source_path(a).await {
            if is_tmpl(&source_ancestor) {
                eprintln!(
                    "\u{26a0} ancestor {} resolves to a chezmoi template (.tmpl). \
                     chezmoi=true requires plain files — use rvpm's Tera templates instead.",
                    a.display(),
                );
                return None;
            }
            let relative = target.strip_prefix(a).ok()?;
            return Some(source_ancestor.join(relative));
        }
        ancestor = a.parent();
    }
    None
}

/// source に書いた後、`chezmoi apply <target>` で target へ反映する。
/// `wrote_to` は実際に書き込んだパス (`write_path` の返り値)。target と
/// 異なる場合のみ apply が必要 (同一なら直接 target に書いたので不要)。
///
/// async + 2 秒 timeout — `chezmoi apply` が hang しても呼び出し側の async
/// runtime をブロックしない (例: 並列 sync 中に 1 プラグインの apply が
/// 詰まっても他の処理は進む)。
pub async fn apply(wrote_to: &Path, target: &Path) {
    if wrote_to == target {
        return;
    }
    let fut = tokio::process::Command::new("chezmoi")
        .arg("apply")
        .arg("--force")
        .arg(target)
        .status();
    match tokio::time::timeout(CHEZMOI_TIMEOUT, fut).await {
        Ok(Ok(s)) if s.success() => {}
        Ok(Ok(s)) => eprintln!(
            "\u{26a0} chezmoi apply {} failed (exit {})",
            target.display(),
            s.code().unwrap_or(-1),
        ),
        Ok(Err(e)) => eprintln!("\u{26a0} chezmoi apply {} failed: {}", target.display(), e),
        Err(_) => eprintln!(
            "\u{26a0} chezmoi apply {} timed out after {}s",
            target.display(),
            CHEZMOI_TIMEOUT.as_secs(),
        ),
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

    #[tokio::test]
    async fn test_write_path_disabled_returns_target() {
        let target = Path::new("/some/target");
        let got = write_path(false, target).await;
        assert_eq!(got, target);
    }

    /// `write_path(true, ...)` が呼ばれても、CI / 開発機に chezmoi が無ければ
    /// `is_chezmoi_available` が即 false を返して target そのまま、また chezmoi
    /// があっても 2s timeout で必ず有限時間で帰ってくる。回帰テストの目的は
    /// 「無限 hang しない」を担保すること。タイムアウト 2s + spawn コスト分の
    /// 余裕を見て 10s で test 自身を打ち切る。
    #[tokio::test]
    async fn test_write_path_enabled_does_not_hang() {
        let target = Path::new("/some/target");
        let fut = write_path(true, target);
        let res = tokio::time::timeout(Duration::from_secs(10), fut).await;
        assert!(
            res.is_ok(),
            "write_path must return within 10s even when chezmoi is missing or slow"
        );
    }
}
