//! chezmoi 連携ヘルパー。
//!
//! `options.chezmoi = true` のとき、rvpm は config.toml や per-plugin hook を
//! **chezmoi source 側に直接書き込み**、`chezmoi apply` で target へ反映する。
//! これにより chezmoi の「source が truth」原則に沿った連携が実現できる。
//!
//! 前提: chezmoi = true のとき、管理対象ファイルは **plain file** であること。
//! `.tmpl` (chezmoi テンプレート) は非対応。rvpm 自身が Tera テンプレートを
//! 持っているため chezmoi のテンプレート機能は不要。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `write_path` / `apply` 1 回あたりに許す全体の wall-clock budget。
/// `run_doctor` の `VERSION_PROBE_TIMEOUT` と同じ思想で、PATH 上の壊れた shim や
/// 応答しない subprocess で rvpm 全体が hang するのを防ぐ。
///
/// `write_path` では `is_chezmoi_available` + `resolve_source_path` (target 1 回 +
/// 祖先 N 回) の合計をこの 1 つの budget でカバーする。これにより、深い階層で
/// 個別 timeout が累積して総待ち時間が桁違いに膨らむことを防ぐ。
const CHEZMOI_TIMEOUT: Duration = Duration::from_secs(2);

/// target パスに対応する chezmoi source パスを解決する純粋ロジック。`probe` を
/// 差し替えることで本番 (`chezmoi source-path` を呼ぶ async 実装) もテスト
/// (HashMap mock) も同じ関数を共有できる。
///
/// 1. `probe(target)` が Some を返せばそのまま使う
/// 2. 返さなければ (新規ファイル等) 祖先を遡り、最初に managed な祖先から
///    相対パスを計算して source 側のフルパスを構築する
/// 3. source パスが `.tmpl` で終わる場合は None (テンプレート非対応)
async fn resolve_source_path<F, Fut>(target: &Path, mut probe: F) -> Option<PathBuf>
where
    F: FnMut(PathBuf) -> Fut,
    Fut: Future<Output = Option<PathBuf>>,
{
    // target 自体が managed なケース (既存ファイル)
    if let Some(sp) = probe(target.to_path_buf()).await {
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
    let mut ancestor = target.parent().map(|p| p.to_path_buf());
    while let Some(a) = ancestor {
        if let Some(source_ancestor) = probe(a.clone()).await {
            if is_tmpl(&source_ancestor) {
                eprintln!(
                    "\u{26a0} ancestor {} resolves to a chezmoi template (.tmpl). \
                     chezmoi=true requires plain files — use rvpm's Tera templates instead.",
                    a.display(),
                );
                return None;
            }
            let relative = target.strip_prefix(&a).ok()?;
            return Some(source_ancestor.join(relative));
        }
        ancestor = a.parent().map(|p| p.to_path_buf());
    }
    None
}

fn is_tmpl(p: &Path) -> bool {
    p.to_string_lossy().ends_with(".tmpl")
}

/// `chezmoi source-path <target>` を実行し、managed なら source パスを返す。
/// 個別タイムアウトは持たない — 呼び出し元の `write_path` が全体 budget を
/// `tokio::time::timeout` でかぶせる前提。
async fn chezmoi_source_path(target: &Path) -> Option<PathBuf> {
    let output = match tokio::process::Command::new("chezmoi")
        .arg("source-path")
        .arg(target)
        .output()
        .await
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

/// `chezmoi --version` で chezmoi が PATH 上に存在し起動可能か確認する。
/// この関数自体はタイムアウトを持たず、呼び出し元の `write_path` の全体 budget で
/// カバーされる。
async fn is_chezmoi_available() -> bool {
    matches!(
        tokio::process::Command::new("chezmoi")
            .arg("--version")
            .output()
            .await,
        Ok(o) if o.status.success(),
    )
}

/// rvpm が書き込むべきパスを返す。chezmoi 有効かつ managed なら source 側、
/// そうでなければ target そのまま。返り値が target と異なれば source に書いた
/// ことを意味するので、呼び出し側は `apply()` を呼んで target へ反映する。
///
/// 内部で `chezmoi --version` と `chezmoi source-path <target/...>` を順次呼ぶが、
/// 全体を **単一の 2 秒タイムアウト** でラップする。深い階層を遡る場合でも待ち
/// 時間が累積せず、`CHEZMOI_TIMEOUT` を超えたら諦めて target をそのまま返す
/// (resilience)。
pub async fn write_path(enabled: bool, target: &Path) -> PathBuf {
    if !enabled {
        return target.to_path_buf();
    }
    let work = async {
        if !is_chezmoi_available().await {
            eprintln!(
                "\u{26a0} options.chezmoi = true but `chezmoi` is not in PATH. \
                 Writing to target directly (install chezmoi or set chezmoi = false).",
            );
            return None;
        }
        resolve_source_path(target, |p| async move { chezmoi_source_path(&p).await }).await
    };
    match tokio::time::timeout(CHEZMOI_TIMEOUT, work).await {
        Ok(Some(sp)) => sp,
        Ok(None) => target.to_path_buf(),
        Err(_) => {
            eprintln!(
                "\u{26a0} chezmoi resolution for {} timed out after {}s; writing to target directly",
                target.display(),
                CHEZMOI_TIMEOUT.as_secs(),
            );
            target.to_path_buf()
        }
    }
}

/// source に書いた後、`chezmoi apply <target>` で target へ反映する。
/// `wrote_to` は実際に書き込んだパス (`write_path` の返り値)。target と
/// 異なる場合のみ apply が必要 (同一なら直接 target に書いたので不要)。
///
/// 単一の `chezmoi apply` 実行を 2 秒タイムアウトで保護する — apply が hang
/// しても呼び出し側の async runtime をブロックしない。
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

    /// HashMap を借用するシンプルな async probe。`resolve_source_path` の
    /// `FnMut(PathBuf) -> Future` 制約を満たす。
    fn mock_probe(
        managed: &HashMap<PathBuf, PathBuf>,
    ) -> impl FnMut(PathBuf) -> std::future::Ready<Option<PathBuf>> + '_ {
        move |p| std::future::ready(managed.get(&p).cloned())
    }

    #[tokio::test]
    async fn test_resolve_existing_managed_file() {
        let target = PathBuf::from("/home/user/.config/rvpm/nvim/config.toml");
        let source =
            PathBuf::from("/home/user/.local/share/chezmoi/dot_config/rvpm/nvim/config.toml");
        let managed = HashMap::from([(target.clone(), source.clone())]);
        let got = resolve_source_path(&target, mock_probe(&managed)).await;
        assert_eq!(got, Some(source));
    }

    #[tokio::test]
    async fn test_resolve_new_file_via_managed_ancestor() {
        let ancestor_target = PathBuf::from("/home/user/.config/rvpm/nvim/plugins");
        let ancestor_source =
            PathBuf::from("/home/user/.local/share/chezmoi/dot_config/rvpm/nvim/plugins");
        let target = ancestor_target.join("github.com/foo/bar/init.lua");
        let managed = HashMap::from([(ancestor_target, ancestor_source.clone())]);
        let got = resolve_source_path(&target, mock_probe(&managed)).await;
        assert_eq!(
            got,
            Some(ancestor_source.join("github.com/foo/bar/init.lua"))
        );
    }

    #[tokio::test]
    async fn test_resolve_tmpl_returns_none() {
        let target = PathBuf::from("/home/user/.config/rvpm/nvim/config.toml");
        let source =
            PathBuf::from("/home/user/.local/share/chezmoi/dot_config/rvpm/nvim/config.toml.tmpl");
        let managed = HashMap::from([(target.clone(), source)]);
        let got = resolve_source_path(&target, mock_probe(&managed)).await;
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn test_resolve_not_managed_returns_none() {
        let target = PathBuf::from("/home/user/.config/rvpm/nvim/config.toml");
        let managed = HashMap::new();
        let got = resolve_source_path(&target, mock_probe(&managed)).await;
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn test_resolve_new_file_no_managed_ancestor_returns_none() {
        let target = PathBuf::from("/home/user/.config/rvpm/nvim/plugins/foo/init.lua");
        let managed = HashMap::new();
        let got = resolve_source_path(&target, mock_probe(&managed)).await;
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
    /// があっても全体 2s timeout で必ず有限時間で帰ってくる。回帰テストの目的は
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
