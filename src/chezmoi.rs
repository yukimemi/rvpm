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
/// - `existed_before = true` (rvpm が mutate する前から存在していたファイル)
///   かつ `path` が chezmoi managed → `ReAdd`、managed じゃない → `Noop`
/// - `existed_before = false` (rvpm が新規作成した。$EDITOR 終了後だと既に
///   ディスク上には存在する点に注意) かつ祖先が chezmoi managed → `Add`
/// - それ以外 → `Noop`
///
/// `existed_before` 引数が必要なのは: `rvpm edit` 等で新規 hook を作ると
/// $EDITOR を抜けた時点で `path.exists() == true` になっており、ファイル
/// 自身は chezmoi にまだ登録されていないので `ReAdd` パスが選ばれて
/// `Noop` に落ち、本来必要な `Add` が一度も発火しない。そのため呼び出し側
/// が mutate 前の存在状態を捕捉して渡す必要がある。
pub fn decide_action<F>(
    enabled: bool,
    existed_before: bool,
    path: &Path,
    mut managed_probe: F,
) -> SyncAction
where
    F: FnMut(&Path) -> bool,
{
    if !enabled {
        return SyncAction::Noop;
    }
    if existed_before {
        if managed_probe(path) {
            SyncAction::ReAdd(path.to_path_buf())
        } else {
            SyncAction::Noop
        }
    } else {
        // 祖先を root 方向へ順に見て、最初に managed なディレクトリが
        // 見つかれば Add。直親が rvpm によって mkdir されたばかりで
        // まだ chezmoi 管理下に無い場合でも、その上 (例えば plugins/)
        // が managed なら `chezmoi add` すれば chezmoi が中間ディレクトリ
        // ごと source に放り込んでくれる。
        let mut current = path.parent();
        while let Some(p) = current {
            if p.exists() && managed_probe(p) {
                return SyncAction::Add(path.to_path_buf());
            }
            current = p.parent();
        }
        SyncAction::Noop
    }
}

/// `chezmoi source-path <p>` を実行して exit 0 なら managed。
/// exit 非 0 は chezmoi が「not managed」と言った場合 (想定内) なので false。
/// 起動自体の IO エラー (PATH 通っていたのに spawn 失敗など) は stderr に
/// warn を出して false を返す (managed と推定するのは危険なので保守的に)。
fn is_managed_via_chezmoi(p: &Path) -> bool {
    match Command::new("chezmoi").arg("source-path").arg(p).output() {
        Ok(o) => o.status.success(),
        Err(e) => {
            eprintln!("\u{26a0} chezmoi source-path {} failed: {}", p.display(), e,);
            false
        }
    }
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
///
/// `existed_before` は mutate/edit を行う **前** の `path.exists()` の値。
/// 新規作成か既存更新かの区別に必要 (詳細は [`decide_action`] 参照)。
pub fn sync(enabled: bool, existed_before: bool, path: &Path) {
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
    let action = decide_action(true, existed_before, path, is_managed_via_chezmoi);
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
    fn test_decide_action_disabled_is_noop() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "").unwrap();
        // 仮に managed_probe が常に true でも、enabled=false なら Noop。
        let got = decide_action(false, true, &file, |_| true);
        assert_eq!(got, SyncAction::Noop);
    }

    #[test]
    fn test_decide_action_existing_managed_is_re_add() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "").unwrap();
        let got = decide_action(true, true, &file, |p| p == file);
        assert_eq!(got, SyncAction::ReAdd(file));
    }

    #[test]
    fn test_decide_action_existing_not_managed_is_noop() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "").unwrap();
        let got = decide_action(true, true, &file, |_| false);
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
        let got = decide_action(true, false, &new_file, move |p| p == plugins_clone);
        assert_eq!(got, SyncAction::Add(new_file));
    }

    #[test]
    fn test_decide_action_new_file_ancestor_not_managed_is_noop() {
        let tmp = tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let new_file = plugins.join("foo").join("init.lua");
        let got = decide_action(true, false, &new_file, |_| false);
        assert_eq!(got, SyncAction::Noop);
    }

    /// `rvpm edit` で新規 hook を作成すると $EDITOR 終了時点で path は既に
    /// 存在する。呼び出し側が existed_before=false を渡しさえすれば Add に
    /// 行く (そうしないと ReAdd 判定で chezmoi source-path が false を返し
    /// Noop に落ちてしまい、Add が一度も発火しない)。
    #[test]
    fn test_decide_action_file_exists_but_was_just_created_is_add() {
        let tmp = tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let new_file = plugins.join("bar").join("init.lua");
        std::fs::create_dir_all(new_file.parent().unwrap()).unwrap();
        std::fs::write(&new_file, "-- new hook").unwrap();
        // ファイル自体は存在するが existed_before=false (rvpm が作った) と伝えると
        // 祖先 plugins/ が managed なら Add になる。
        let plugins_clone = plugins.clone();
        let got = decide_action(true, false, &new_file, move |p| p == plugins_clone);
        assert_eq!(got, SyncAction::Add(new_file));
    }
}
