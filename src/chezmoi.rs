//! chezmoi 連携ヘルパー。
//!
//! `options.chezmoi = true` のとき、rvpm が `config.toml` や per-plugin hook を
//! 書き換えた後にこのモジュールの `sync()` を呼ぶと、chezmoi の source 側へ
//! `chezmoi re-add` (既存 managed) / `chezmoi add` (新規ファイル、祖先 managed)
//! で自動同期する。
//!
//! `chezmoi` コマンドが見つからない / 非 0 終了した場合は stderr に 1 行 warn を
//! 出して処理は継続する (resilience 原則)。

use std::path::{Path, PathBuf};
use std::process::Command;

/// chezmoi コマンドに何をさせるかを決めた結果。`sync()` の内部ロジックを
/// 副作用 (Command 実行) から分離するための enum。テスト容易性のため公開。
#[derive(Debug, PartialEq, Eq)]
pub enum SyncAction {
    /// 何もしない (feature OFF、rvpm 外のパス、managed 祖先なし等)。
    Noop,
    /// 既存 managed ファイルの再同期: `chezmoi re-add <path>`
    ReAdd(PathBuf),
    /// 新規ファイルの初回登録: `chezmoi add <path>`
    Add(PathBuf),
}

/// `path` に対して行うべき chezmoi 操作を決定する。Command は実行しない。
///
/// - `enabled` が false → `Noop`
/// - `path` が既に存在し、かつ chezmoi managed → `ReAdd`
/// - `path` が存在せず、最初に存在する祖先が chezmoi managed → `Add`
/// - それ以外 → `Noop`
///
/// `managed_probe` は「指定パスが chezmoi managed か」を返すクロージャ。
/// 本番は `chezmoi source-path <p>` の exit code を見るが、テストでは
/// 任意の判定関数を差し込める。
pub fn decide_action<F>(enabled: bool, path: &Path, mut managed_probe: F) -> SyncAction
where
    F: FnMut(&Path) -> bool,
{
    if !enabled {
        return SyncAction::Noop;
    }
    if path.exists() {
        if managed_probe(path) {
            SyncAction::ReAdd(path.to_path_buf())
        } else {
            SyncAction::Noop
        }
    } else {
        match first_existing_ancestor(path) {
            Some(anc) if managed_probe(&anc) => SyncAction::Add(path.to_path_buf()),
            _ => SyncAction::Noop,
        }
    }
}

/// 指定パスから root に向かって遡り、最初に **存在する** ディレクトリを返す。
/// 新規ファイル作成時、どこまで遡れば既存の親があるかを調べるのに使う。
pub fn first_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(p) = current {
        if p.exists() {
            return Some(p.to_path_buf());
        }
        current = p.parent();
    }
    None
}

/// `chezmoi source-path <p>` を実行して exit 0 なら managed。
fn is_managed_via_chezmoi(p: &Path) -> bool {
    Command::new("chezmoi")
        .arg("source-path")
        .arg(p)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `chezmoi` 実行可能ファイルが PATH に存在するか。`--version` で軽く叩いて判定。
fn is_chezmoi_available() -> bool {
    Command::new("chezmoi")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `options.chezmoi = true` のときに rvpm の mutate 系コマンドから呼ばれる
/// エントリポイント。`path` に対する必要な chezmoi 操作を判定し実行する。
/// 失敗しても rvpm 本体の処理は止めない (stderr に warn 1 行だけ出して継続)。
///
/// `enabled = true` なのに `chezmoi` バイナリが PATH に無い場合は、ユーザーが
/// 明示的に ON にしている以上不整合なので毎回 warn を出す (設定ミスを放置
/// しない方が親切)。鬱陶しければ `options.chezmoi = false` に戻すか、
/// `chezmoi` を入れるかを選んでもらう。
pub fn sync(enabled: bool, path: &Path) {
    if !enabled {
        return;
    }
    if !is_chezmoi_available() {
        eprintln!(
            "\u{26a0} options.chezmoi = true but `chezmoi` is not in PATH. \
             Skipping sync for {} (install chezmoi or set chezmoi = false).",
            path.display(),
        );
        return;
    }
    let action = decide_action(true, path, is_managed_via_chezmoi);
    match action {
        SyncAction::Noop => {}
        SyncAction::ReAdd(p) => run_chezmoi(&["re-add"], &p),
        SyncAction::Add(p) => run_chezmoi(&["add"], &p),
    }
}

fn run_chezmoi(args: &[&str], path: &Path) {
    let mut cmd = Command::new("chezmoi");
    cmd.args(args).arg(path);
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!(
            "\u{26a0} chezmoi {} {} failed (exit {})",
            args.join(" "),
            path.display(),
            s.code().unwrap_or(-1),
        ),
        Err(e) => eprintln!(
            "\u{26a0} chezmoi {} {} could not be spawned: {}",
            args.join(" "),
            path.display(),
            e,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_first_existing_ancestor_returns_self_when_path_exists() {
        let tmp = tempdir().unwrap();
        let got = first_existing_ancestor(tmp.path()).unwrap();
        assert_eq!(got, tmp.path());
    }

    #[test]
    fn test_first_existing_ancestor_walks_up_through_missing_dirs() {
        let tmp = tempdir().unwrap();
        let nested = tmp.path().join("a").join("b").join("c").join("init.lua");
        // `nested` も `a/b/c/` も存在しない。tmp.path() は存在する。
        let got = first_existing_ancestor(&nested).unwrap();
        assert_eq!(got, tmp.path());
    }

    #[test]
    fn test_first_existing_ancestor_stops_at_first_existing_intermediate() {
        let tmp = tempdir().unwrap();
        let mid = tmp.path().join("plugins");
        std::fs::create_dir_all(&mid).unwrap();
        let nested = mid
            .join("github.com")
            .join("foo")
            .join("bar")
            .join("init.lua");
        let got = first_existing_ancestor(&nested).unwrap();
        assert_eq!(got, mid);
    }

    #[test]
    fn test_decide_action_disabled_is_noop() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "").unwrap();
        // 仮に managed_probe が常に true でも、enabled=false なら Noop。
        let got = decide_action(false, &file, |_| true);
        assert_eq!(got, SyncAction::Noop);
    }

    #[test]
    fn test_decide_action_existing_managed_is_re_add() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "").unwrap();
        let got = decide_action(true, &file, |p| p == file);
        assert_eq!(got, SyncAction::ReAdd(file));
    }

    #[test]
    fn test_decide_action_existing_not_managed_is_noop() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "").unwrap();
        let got = decide_action(true, &file, |_| false);
        assert_eq!(got, SyncAction::Noop);
    }

    #[test]
    fn test_decide_action_new_file_ancestor_managed_is_add() {
        let tmp = tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let new_file = plugins
            .join("github.com")
            .join("foo")
            .join("bar")
            .join("init.lua");
        // plugins/ だけを managed として扱う。祖先遡りが効いているはず。
        let plugins_clone = plugins.clone();
        let got = decide_action(true, &new_file, move |p| p == plugins_clone);
        assert_eq!(got, SyncAction::Add(new_file));
    }

    #[test]
    fn test_decide_action_new_file_ancestor_not_managed_is_noop() {
        let tmp = tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let new_file = plugins.join("foo").join("init.lua");
        let got = decide_action(true, &new_file, |_| false);
        assert_eq!(got, SyncAction::Noop);
    }
}
