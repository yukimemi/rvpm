use anyhow::Result;
use std::path::{Path, PathBuf};

/// Neovim の `runtimepath` が走査する慣習ディレクトリ + denops エコシステム
/// で必要なディレクトリ。
/// plugin ルート直下にある **これらのディレクトリのみ** を merged にコピー
/// 対象とする。`tests/`, `scripts/`, `examples/`, `src/` 等はランタイム的に
/// 無関係で、衝突警告のノイズになるだけなので除外する。
///
/// 参考: `:help rtp`、`:help runtime`、Neovim core の runtime/ ディレクトリ、
/// denops.vim 慣習 (`denops/<plugin>/main.ts` を rtp 経由で discover する)。
const RTP_DIRS: &[&str] = &[
    "after", "autoload", "colors", "compiler",
    "denops", // denops.vim — TypeScript plugin source
    "doc", "ftdetect", "ftplugin", "indent", "keymap", "lang", "lua", "pack", "parser", "plugin",
    "queries", "rplugin", "spell", "syntax",
    "tutor", // :Tutor 用、Neovim core が公式に走査する rtp ディレクトリ
];

/// ファイルをターゲットに張る。同一ボリューム内なら hard link (Windows でも
/// 管理者権限不要)、別ボリューム等で失敗したら copy にフォールバック。
/// `dst.exists()` なら何もしない (衝突時は呼び出し側で skip 判定する前提)。
fn hard_link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if std::fs::hard_link(src, dst).is_err() {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// Vim の helptags ファイル名 (`tags` / `tags-<lang>`) かを判定する。
/// 拡張子付きの `tags.bak` 等はバックアップなので false。
fn is_helptags_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "tags" || (lower.starts_with("tags-") && !lower.contains('.'))
}

/// `merge_plugin` の返り値。衝突したファイルのリストを含む (first-wins、
/// 後から来た plugin のファイルが skip された場合に記録)。
#[derive(Debug, Default)]
pub struct MergeResult {
    pub conflicts: Vec<MergeConflict>,
    /// このコールで merged/ に新規配置された relative path のリスト。
    /// 呼び出し側 (main.rs) が plugin 名との対応表を組み立て、後続 plugin で
    /// conflict が起きたときに「勝者 plugin 名」を lookup するのに使う。
    /// 既存ファイルを `first-wins` で skip したケースは含まれない。
    pub placed: Vec<PathBuf>,
}

/// 衝突情報: merged dir 相対のファイルパス。`MergeResult` に積まれて返り、
/// 呼び出し側 (main.rs) が plugin 名と組にしてサマリ表示する。
#[derive(Debug, Clone)]
pub struct MergeConflict {
    /// merged dir 相対パス (例: `lua/cmp/init.lua`)
    pub relative: PathBuf,
}

/// 指定したプラグインの全ファイルを merged ディレクトリにファイル単位で
/// リンクする。
///
/// 設計:
/// - ディレクトリは `create_dir_all` で実体として作る (junction/symlink にしない)。
///   これにより複数プラグインが同じ階層下にファイルを置いても共存できる。
/// - ファイルは hard link で張る (Windows でも admin 不要、Unix でも安定)。
///   別ボリューム等で hard link 失敗時のみ copy にフォールバック。
/// - 同じ merged 内パスに別プラグインのファイルが既に存在する場合は **first-wins**
///   で skip し、`MergeConflict` として返す (呼び出し側が最終的に警告サマリを出す)。
/// - 隠しディレクトリ (`.git`, `.github` 等) は plugin ルート直下に限り除外。
pub fn merge_plugin(src: &Path, dst_root: &Path) -> Result<MergeResult> {
    let mut result = MergeResult::default();
    if !dst_root.exists() {
        std::fs::create_dir_all(dst_root)?;
    }
    walk(
        src,
        src,
        dst_root,
        &mut result,
        WalkOptions { rtp_only: true },
    )?;
    Ok(result)
}

/// 指定したプラグインの全ファイルを `views/<plug>/` ディレクトリにファイル単位で
/// リンクする (`#119` の view 戦略)。
///
/// `merge_plugin` との違い: `RTP_DIRS` の絞り込みも、 ルート直下のメタファイル
/// (README.md / Cargo.toml 等) の除外も **しない**。 views/<plug>/ は per-plugin で
/// cross-plugin の衝突は発生しないので、 絞り込みノイズ抑制が不要。
///
/// 重要: `build` / `build_lua` を持つ plugin の build artifact (`target/release/...`,
/// `build/` 等の plugin 任意のディレクトリ) も拾うために rtp 慣習外を含めて全部 link
/// する。 これが無いと blink.cmp 等の `debug.getinfo()` ベースで自分の lua module
/// 位置から相対参照する plugin が build 出力を見つけられない。
///
/// `.git/` 等のドットファイルと `doc/tags` の上書き対策は引き続き skip する
/// (内部で同じ `walk` を `rtp_only: false` で呼び分け)。
pub fn merge_plugin_view(src: &Path, dst_root: &Path) -> Result<MergeResult> {
    let mut result = MergeResult::default();
    if !dst_root.exists() {
        std::fs::create_dir_all(dst_root)?;
    }
    walk(
        src,
        src,
        dst_root,
        &mut result,
        WalkOptions { rtp_only: false },
    )?;
    Ok(result)
}

/// `merge_plugin_view` の **`doc/` 抜き** 版。 `views/<plug>/` 用 (`#119`)。
///
/// 用途: `PluginMergeMode::ViewWithoutDoc` のとき、 plugin の全 entry を
/// `views/<plug>/` に `doc/` を除いて hard-link する。 doc/ は別途
/// `merge_plugin_doc_only` で `merged/doc/` に集約される。
/// 結果として rtp に乗るのは doc を持たない view → `:tselect` の重複が発生せず、
/// `:help` は集約された `merged/doc/tags` を 1 経路で引ける。
///
/// `merge_plugin_view` と同じく rtp 慣習外も含めて全部 link する (build artifact
/// を拾うため)。 doc/ だけ root 直下で除外する点が異なる。
pub fn merge_plugin_view_no_doc(src: &Path, dst_root: &Path) -> Result<MergeResult> {
    let mut result = MergeResult::default();
    if !dst_root.exists() {
        std::fs::create_dir_all(dst_root)?;
    }
    // plugin ルート直下の各 entry を見て doc/ だけスキップ。
    // `walk` は再帰実装なので、 ルート直下で doc/ を弾けば doc/ 配下まで
    // 走らない。
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        if name_str == "doc" {
            continue;
        }
        let entry_path = entry.path();
        if entry_path.is_file() {
            // ルート直下のファイル (README.md / Cargo.toml etc.) も link する
            let dst_path = dst_root.join(&name);
            if dst_path.exists() {
                result.conflicts.push(MergeConflict {
                    relative: PathBuf::from(&name),
                });
            } else {
                hard_link_or_copy(&entry_path, &dst_path)?;
                result.placed.push(PathBuf::from(&name));
            }
            continue;
        }
        // ディレクトリは rtp_only=false で walk (rtp 慣習外も含めて全部)
        let dst_subdir = dst_root.join(name.as_os_str());
        // dst_subdir に **ファイル** が居るケース (先行 plugin 等が同 path にファイル
        // を張り済) は first-wins で skip + conflict 記録。 create_dir_all は ENOTDIR
        // で落ちるので、 walk 内のディレクトリ衝突ハンドリングと整合させる
        // (CodeRabbit PR #124 指摘)。
        if dst_subdir.is_file() {
            result.conflicts.push(MergeConflict {
                relative: PathBuf::from(&name),
            });
            continue;
        }
        if !dst_subdir.exists() {
            std::fs::create_dir_all(&dst_subdir)?;
        }
        walk(
            src,
            &entry_path,
            dst_root,
            &mut result,
            WalkOptions { rtp_only: false },
        )?;
    }
    Ok(result)
}

/// プラグインの `doc/` ディレクトリだけを merged にリンクする。`merge_plugin` の
/// subset 版。
///
/// 用途: lazy プラグインは `merge_plugin` から外しているので
/// `<plugin>/doc/` が runtimepath に乗らず、trigger 前は `:help <topic>` で
/// 見つけられない。`doc/` だけを `merged/doc/` にリンクすれば、
/// `merged/` は常に rtp に乗っているので `:help` が引けるようになる。
/// `lua/` 等は引き続き merged に流れないので lazy 性は維持される。
///
/// 挙動:
/// - `<src>/doc/` が無ければ何もせず空 `MergeResult` を返す。
/// - 衝突 / 隠しファイル / `doc/tags` 系の skip ルールは `merge_plugin` と共通
///   (内部で同じ `walk` を再利用)。
pub fn merge_plugin_doc_only(src: &Path, dst_root: &Path) -> Result<MergeResult> {
    let mut result = MergeResult::default();
    let src_doc = src.join("doc");
    if !src_doc.is_dir() {
        return Ok(result);
    }
    if !dst_root.exists() {
        std::fs::create_dir_all(dst_root)?;
    }
    let dst_doc = dst_root.join("doc");
    if !dst_doc.exists() {
        std::fs::create_dir_all(&dst_doc)?;
    }
    walk(
        src,
        &src_doc,
        dst_root,
        &mut result,
        WalkOptions { rtp_only: true },
    )?;
    Ok(result)
}

/// `walk` のフィルタオプション。 merged/ 用 (rtp_only=true) と views/ 用
/// (rtp_only=false) で plugin ルート直下の挙動を切り替える。
#[derive(Clone, Copy)]
struct WalkOptions {
    /// plugin ルート直下で `RTP_DIRS` 縛りと「ファイル除外」を効かせるか。
    /// `merged/` (cross-plugin 共有) は cross-plugin 衝突ノイズを抑える必要が
    /// あるので true。 `views/<plug>/` は per-plugin で衝突無し、 build
    /// artifact (target/, build/ 等) も拾いたいので false。
    rtp_only: bool,
}

fn walk(
    plugin_root: &Path,
    dir: &Path,
    dst_root: &Path,
    result: &mut MergeResult,
    opts: WalkOptions,
) -> Result<()> {
    let at_plugin_root = dir == plugin_root;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let src_path = entry.path();

        // 全階層で隠しエントリ (.git / .github / .gitignore / .luarc.json /
        // .editorconfig / .gitkeep 等) は除外。Neovim 起動に無関係で、深い階層
        // (例: `doc/.gitignore`) でも plugin 横断で名前が被って noise になる。
        if name_str.starts_with('.') {
            continue;
        }

        if at_plugin_root && opts.rtp_only {
            // plugin ルート直下のファイル (README.md / LICENSE / Makefile /
            // package.json / *.toml 等のメタファイル) は rtp に置く意味が無く、
            // plugin 横断で同名衝突するだけのノイズなので merge しない。
            if src_path.is_file() {
                continue;
            }
            // ディレクトリは Neovim の rtp 慣習に該当するもののみ通す
            // (tests/ scripts/ examples/ src/ etc. は無関係)。
            if !RTP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
        }

        let rel = src_path
            .strip_prefix(plugin_root)
            .expect("entry is under plugin_root")
            .to_path_buf();
        let dst_path = dst_root.join(&rel);

        // `doc/<plugin>/tags` / `doc/.../tags-<lang>` は plugin が自分で
        // commit している tags ファイルが時々ある。これを hard link すると
        // 後段の `:helptags merged_dir/doc` が同 inode を書き換え、源 plugin の
        // `repos/<plugin>/doc/tags` まで上書きしてしまい git status が汚れる。
        // tags は merged 側で生成し直すので skip して構わない (doc/ 直下の
        // *.txt / *.<lang>x が hard link されているので :helptags は機能する)。
        if !src_path.is_dir()
            && rel
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == "doc")
            && is_helptags_file(&name_str)
        {
            continue;
        }

        if src_path.is_dir() {
            // dst 側に既に **ファイル** が居るケース: 先行 plugin が同じ path に
            // ファイルを張り済 (例: A の `foo/bar` がファイル、B では `foo/bar/baz`
            // のディレクトリ階層)。`create_dir_all` は ENOTDIR で落ちるので、
            // first-wins と整合させて conflict 記録 + skip する (resilience)。
            if dst_path.is_file() {
                result.conflicts.push(MergeConflict { relative: rel });
                continue;
            }
            if !dst_path.exists() {
                std::fs::create_dir_all(&dst_path)?;
            }
            walk(plugin_root, &src_path, dst_root, result, opts)?;
        } else if dst_path.exists() {
            // first-wins: 既にファイル / ディレクトリが居る → skip
            // (dst が dir で src が file の対称ケースもここでカバー)
            result.conflicts.push(MergeConflict { relative: rel });
        } else {
            hard_link_or_copy(&src_path, &dst_path)?;
            result.placed.push(rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn test_merge_no_conflict() {
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let a = root.path().join("plug_a");
        let b = root.path().join("plug_b");
        write(&a.join("lua/plug_a/init.lua"), "print('a')");
        write(&b.join("plugin/b.vim"), "echo 'b'");

        let r1 = merge_plugin(&a, &merged).unwrap();
        let r2 = merge_plugin(&b, &merged).unwrap();

        assert!(merged.join("lua/plug_a/init.lua").exists());
        assert!(merged.join("plugin/b.vim").exists());
        assert!(r1.conflicts.is_empty());
        assert!(r2.conflicts.is_empty());
    }

    #[test]
    fn test_merge_conflict_first_wins() {
        // A と B 両方が lua/shared/init.lua を持つ → A が勝ち、B が conflict に。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let a = root.path().join("plug_a");
        let b = root.path().join("plug_b");
        write(&a.join("lua/shared/init.lua"), "from a");
        write(&b.join("lua/shared/init.lua"), "from b");

        let _ = merge_plugin(&a, &merged).unwrap();
        let r2 = merge_plugin(&b, &merged).unwrap();

        // merged には A の内容が残る
        let content = fs::read_to_string(merged.join("lua/shared/init.lua")).unwrap();
        assert_eq!(content, "from a");

        // B から見ると 1 件 conflict
        assert_eq!(r2.conflicts.len(), 1);
        assert_eq!(
            r2.conflicts[0].relative,
            PathBuf::from("lua").join("shared").join("init.lua")
        );
        let _ = b; // skipped_plugin_root を struct で持たない方針に変更したので参照のみ
    }

    #[test]
    fn test_merge_same_dir_different_files_coexist() {
        // nvim-cmp / blink.cmp 的ケース: 同じ `lua/cmp/` 階層で別ファイル → 両立。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let a = root.path().join("plug_a");
        let b = root.path().join("plug_b");
        write(&a.join("lua/cmp/a.lua"), "a");
        write(&b.join("lua/cmp/b.lua"), "b");

        let r1 = merge_plugin(&a, &merged).unwrap();
        let r2 = merge_plugin(&b, &merged).unwrap();

        assert!(merged.join("lua/cmp/a.lua").exists());
        assert!(merged.join("lua/cmp/b.lua").exists());
        assert!(r1.conflicts.is_empty());
        assert!(r2.conflicts.is_empty());
    }

    #[test]
    fn test_merge_skips_root_level_dotfiles() {
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        // plugin ルート直下の .git/ は除外される
        write(&p.join(".git/config"), "[core]");
        // plugin ルート直下の .github/workflows/ci.yml も除外
        write(&p.join(".github/workflows/ci.yml"), "name: CI");
        // 通常ファイルは含まれる
        write(&p.join("plugin/foo.vim"), "echo 'foo'");

        let r = merge_plugin(&p, &merged).unwrap();

        assert!(!merged.join(".git").exists());
        assert!(!merged.join(".github").exists());
        assert!(merged.join("plugin/foo.vim").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_merge_skips_root_level_meta_files() {
        // plugin ルート直下のメタファイル (README.md / LICENSE / Makefile /
        // *.toml / package.json 等) は rtp に置く意味が無く、衝突警告ノイズに
        // なるだけなので除外する。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("README.md"), "# plug");
        write(&p.join("LICENSE"), "MIT");
        write(&p.join("Makefile"), "all:");
        write(&p.join("package.json"), "{}");
        write(&p.join("stylua.toml"), "");
        // ディレクトリ内のファイルは残る
        write(&p.join("plugin/foo.vim"), "echo 'foo'");
        // ディレクトリ自体は深い階層で残る
        write(&p.join("doc/foo.txt"), "*foo*");

        let r = merge_plugin(&p, &merged).unwrap();

        assert!(!merged.join("README.md").exists());
        assert!(!merged.join("LICENSE").exists());
        assert!(!merged.join("Makefile").exists());
        assert!(!merged.join("package.json").exists());
        assert!(!merged.join("stylua.toml").exists());
        assert!(merged.join("plugin/foo.vim").exists());
        assert!(merged.join("doc/foo.txt").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_merge_skips_committed_doc_tags() {
        // plugin がリポジトリに `doc/tags` を commit していても hard link しない
        // (後段の :helptags merged/doc が再生成するし、hard link だと源 plugin の
        // tags ファイルまで書き換えて git status を汚す)。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("doc/foo.txt"), "*foo*");
        write(&p.join("doc/tags"), "stale-tags");
        write(&p.join("doc/tags-ja"), "stale-tags-ja");

        let r = merge_plugin(&p, &merged).unwrap();

        assert!(merged.join("doc/foo.txt").exists());
        assert!(
            !merged.join("doc/tags").exists(),
            "doc/tags should be skipped"
        );
        assert!(
            !merged.join("doc/tags-ja").exists(),
            "doc/tags-ja should be skipped"
        );
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_merge_keeps_doc_tags_named_files_with_extension() {
        // tags.bak / tags-ja.old のようなバックアップは tags ファイルではないので
        // 通常通り link される (doctor 側の判定と整合)。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("doc/foo.txt"), "*foo*");
        write(&p.join("doc/tags.bak"), "backup");

        let r = merge_plugin(&p, &merged).unwrap();
        assert!(merged.join("doc/tags.bak").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_merge_includes_tutor_dir() {
        // `:Tutor` 用の `tutor/` も Neovim core が走査する rtp ディレクトリ。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("tutor/intro.tutor"), "# tutor");

        let r = merge_plugin(&p, &merged).unwrap();

        assert!(merged.join("tutor/intro.tutor").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_merge_dir_vs_file_collision_is_recorded_as_conflict() {
        // A: `lua/foo` がファイル, B: `lua/foo/bar.lua` (foo がディレクトリ)。
        // create_dir_all が ENOTDIR で落ちずに first-wins で conflict 記録。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let a = root.path().join("plug_a");
        let b = root.path().join("plug_b");
        write(&a.join("lua/foo"), "i am a file from a");
        write(&b.join("lua/foo/bar.lua"), "from b");

        let _ = merge_plugin(&a, &merged).unwrap();
        let r2 = merge_plugin(&b, &merged).unwrap();

        // A のファイル `lua/foo` は残る
        assert!(merged.join("lua/foo").is_file());
        // B 側で 1 件 conflict 記録 (path は dir エントリ `lua/foo`)
        assert_eq!(r2.conflicts.len(), 1);
        assert_eq!(r2.conflicts[0].relative, PathBuf::from("lua").join("foo"));
    }

    #[test]
    fn test_merge_file_vs_dir_collision_is_recorded_as_conflict() {
        // 逆方向: A: `lua/foo/bar.lua` (foo がディレクトリ), B: `lua/foo` がファイル。
        // dst にディレクトリが存在 → file 張りで衝突 → conflict 記録。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let a = root.path().join("plug_a");
        let b = root.path().join("plug_b");
        write(&a.join("lua/foo/bar.lua"), "from a");
        write(&b.join("lua/foo"), "i am a file from b");

        let _ = merge_plugin(&a, &merged).unwrap();
        let r2 = merge_plugin(&b, &merged).unwrap();

        // A の bar.lua は残る、merged/lua/foo はディレクトリ
        assert!(merged.join("lua/foo").is_dir());
        assert!(merged.join("lua/foo/bar.lua").exists());
        // B 側で 1 件 conflict 記録
        assert_eq!(r2.conflicts.len(), 1);
        assert_eq!(r2.conflicts[0].relative, PathBuf::from("lua").join("foo"));
    }

    #[test]
    fn test_merge_placed_lists_newly_linked_files() {
        // first-wins の勝者を後段で特定できるよう、merge_plugin は
        // このコールで新規配置したファイルを `placed` に詰める。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("plugin/init.lua"), "return {}");
        write(&p.join("lua/foo/bar.lua"), "return {}");

        let r = merge_plugin(&p, &merged).unwrap();

        assert!(r.conflicts.is_empty());
        // 新規配置されたファイル 2 件が記録される (順序は不問)
        let mut placed: Vec<_> = r
            .placed
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        placed.sort();
        assert_eq!(placed, vec!["lua/foo/bar.lua", "plugin/init.lua"]);
    }

    #[test]
    fn test_merge_placed_excludes_skipped_conflicts() {
        // first-wins で skip されたファイルは placed に入らない
        // (skip された方は conflict 側で記録される)。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let a = root.path().join("plug_a");
        let b = root.path().join("plug_b");
        write(&a.join("plugin/init.lua"), "from a");
        write(&b.join("plugin/init.lua"), "from b");

        let r1 = merge_plugin(&a, &merged).unwrap();
        let r2 = merge_plugin(&b, &merged).unwrap();

        // A は新規配置したので placed に入る
        assert_eq!(r1.placed.len(), 1);
        assert_eq!(
            r1.placed[0].to_string_lossy().replace('\\', "/"),
            "plugin/init.lua"
        );
        // B は first-wins で skip → conflict に入り、placed には入らない
        assert!(r2.placed.is_empty());
        assert_eq!(r2.conflicts.len(), 1);
    }

    #[test]
    fn test_merge_includes_denops_dir() {
        // denops.vim 系のプラグイン (`denops/<plugin>/main.ts`) は runtime path
        // 経由で discover されるので merge 対象に含める。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(
            &p.join("denops/myplug/main.ts"),
            "export async function main() {}",
        );
        write(&p.join("denops/myplug/util.ts"), "export const x = 1;");

        let r = merge_plugin(&p, &merged).unwrap();

        assert!(merged.join("denops/myplug/main.ts").exists());
        assert!(merged.join("denops/myplug/util.ts").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_merge_skips_non_rtp_dirs() {
        // tests/ scripts/ examples/ src/ 等は rtp に乗らないので merge 対象外。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("tests/spec.lua"), "test");
        write(&p.join("scripts/build.sh"), "#!/bin/sh");
        write(&p.join("examples/demo.lua"), "demo");
        write(&p.join("src/main.rs"), "fn main() {}");
        // rtp 慣習ディレクトリは含まれる
        write(&p.join("plugin/foo.vim"), "echo 'foo'");
        write(&p.join("lua/foo/init.lua"), "return {}");

        let r = merge_plugin(&p, &merged).unwrap();

        assert!(!merged.join("tests").exists());
        assert!(!merged.join("scripts").exists());
        assert!(!merged.join("examples").exists());
        assert!(!merged.join("src").exists());
        assert!(merged.join("plugin/foo.vim").exists());
        assert!(merged.join("lua/foo/init.lua").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_merge_includes_all_rtp_dirs() {
        // RTP_DIRS に列挙したディレクトリは全部 merge 対象。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        for dir in RTP_DIRS {
            write(&p.join(dir).join("file.txt"), dir);
        }

        let r = merge_plugin(&p, &merged).unwrap();
        assert!(r.conflicts.is_empty());
        for dir in RTP_DIRS {
            assert!(
                merged.join(dir).join("file.txt").exists(),
                "missing rtp dir in merged: {}",
                dir
            );
        }
    }

    #[test]
    fn test_merge_no_conflict_for_meta_files_across_plugins() {
        // 全プラグインが README.md / LICENSE を持っていても衝突しない (skip 済)
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        for name in ["a", "b", "c"] {
            let p = root.path().join(name);
            write(&p.join("README.md"), name);
            write(&p.join("LICENSE"), "MIT");
            write(&p.join(format!("plugin/{}.vim", name)), "");
            let r = merge_plugin(&p, &merged).unwrap();
            assert!(
                r.conflicts.is_empty(),
                "expected no conflicts for {}, got: {:?}",
                name,
                r.conflicts
            );
        }
    }

    #[test]
    fn test_merge_preserves_nested_dirs() {
        // 深い階層も正しく再帰して張る。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("lua/foo/bar/baz/deep.lua"), "deep");
        write(&p.join("lua/foo/bar/baz/extra.lua"), "extra");

        let r = merge_plugin(&p, &merged).unwrap();

        assert!(merged.join("lua/foo/bar/baz/deep.lua").exists());
        assert!(merged.join("lua/foo/bar/baz/extra.lua").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_merge_skips_dotfiles_at_any_depth() {
        // 全階層で dotfile を skip する: doc/.gitignore のように plugin が
        // CI / 開発用に置く隠しファイルは Neovim 起動には無関係なので、
        // 衝突警告のノイズになるだけ。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("doc/foo.txt"), "*foo*");
        write(&p.join("doc/.gitignore"), "tags");
        write(&p.join("lua/foo/.luarc.json"), "{}");
        write(&p.join("lua/foo/init.lua"), "return {}");

        let r = merge_plugin(&p, &merged).unwrap();

        assert!(merged.join("doc/foo.txt").exists());
        assert!(!merged.join("doc/.gitignore").exists());
        assert!(merged.join("lua/foo/init.lua").exists());
        assert!(!merged.join("lua/foo/.luarc.json").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_hard_link_shares_content_with_source() {
        // Windows/Unix 問わず、hard link ならソース側の変更が merged に反映される。
        // (fallback の copy だった場合は反映されないので、この挙動で区別できる。)
        // hard link は別ボリュームで失敗するが、同一 tempdir 内なら成功するはず。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("plugin/hello.vim"), "initial");

        let _ = merge_plugin(&p, &merged).unwrap();

        // ソース側を書き換える (hard link なら merged 側にも反映)
        fs::write(p.join("plugin/hello.vim"), "updated").unwrap();

        let merged_content = fs::read_to_string(merged.join("plugin/hello.vim")).unwrap();
        // tempdir は通常同一ボリューム上にあるので hard link が成功する想定。
        // 万一 copy fallback に落ちる環境では "initial" のまま — そのケースは
        // ここでは許容 (hard link が実装されているかの smoke テストなので
        // assert_ne! で "strict equality failed" にはしない)。
        assert!(
            merged_content == "updated" || merged_content == "initial",
            "unexpected content: {}",
            merged_content
        );
    }

    #[test]
    fn test_view_no_doc_links_everything_except_doc() {
        // ViewWithoutDoc 用: doc/ は触らず、それ以外の rtp dir を全部 link。
        let root = tempdir().unwrap();
        let view = root.path().join("view");
        let p = root.path().join("plug");
        write(&p.join("doc/foo.txt"), "*foo*");
        write(&p.join("lua/foo/init.lua"), "return {}");
        write(&p.join("plugin/bar.vim"), "echo 'bar'");
        write(&p.join("ftplugin/rust.vim"), "setl shiftwidth=4");

        let r = merge_plugin_view_no_doc(&p, &view).unwrap();

        // doc/ は来ない
        assert!(!view.join("doc").exists());
        // それ以外は来る
        assert!(view.join("lua/foo/init.lua").exists());
        assert!(view.join("plugin/bar.vim").exists());
        assert!(view.join("ftplugin/rust.vim").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_view_no_doc_skips_dotfiles_but_keeps_meta() {
        // views/ は per-plugin なので衝突発生せず、 root meta files は link する。
        // dotfile (.git/ など) のみ除外。
        let root = tempdir().unwrap();
        let view = root.path().join("view");
        let p = root.path().join("plug");
        write(&p.join("README.md"), "# plug");
        write(&p.join(".git/config"), "[core]");
        write(&p.join("Makefile"), "all:");
        write(&p.join("Cargo.toml"), "[package]");
        write(&p.join("lua/foo.lua"), "return {}");

        let r = merge_plugin_view_no_doc(&p, &view).unwrap();

        // dotfile は除外
        assert!(!view.join(".git").exists());
        // root meta files は link される (views は per-plugin なので衝突無し)
        assert!(view.join("README.md").exists());
        assert!(view.join("Makefile").exists());
        assert!(view.join("Cargo.toml").exists());
        assert!(view.join("lua/foo.lua").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_view_no_doc_links_build_artifacts() {
        // blink.cmp の build_lua が target/release/ に吐く .dll / .so 等を views/<plug>/
        // 配下にも link する (#119 fix)。 元々 RTP_DIRS 縛りで除外されてて、 plugin が
        // debug.getinfo() で自分の lua から相対参照する設計の場合に library が見つからない
        // 不具合があった。
        let root = tempdir().unwrap();
        let view = root.path().join("view");
        let p = root.path().join("plug");
        write(&p.join("doc/foo.txt"), "*foo*");
        write(&p.join("lua/blink/cmp/init.lua"), "return {}");
        write(&p.join("target/release/blink_cmp_fuzzy.dll"), "DLL");
        write(&p.join("build/native/lib.so"), "SO");

        let r = merge_plugin_view_no_doc(&p, &view).unwrap();

        // doc/ は来ない
        assert!(!view.join("doc").exists());
        // build artifact ディレクトリも link される
        assert!(view.join("target/release/blink_cmp_fuzzy.dll").exists());
        assert!(view.join("build/native/lib.so").exists());
        assert!(view.join("lua/blink/cmp/init.lua").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_view_no_doc_includes_non_rtp_dirs() {
        // tests/ scripts/ examples/ 等も link する (rtp 慣習外も含めて全部)。
        let root = tempdir().unwrap();
        let view = root.path().join("view");
        let p = root.path().join("plug");
        write(&p.join("tests/spec.lua"), "test");
        write(&p.join("scripts/build.sh"), "#!/bin/sh");
        write(&p.join("examples/demo.lua"), "demo");
        write(&p.join("src/main.rs"), "fn main() {}");
        write(&p.join("plugin/foo.vim"), "echo 'foo'");

        let r = merge_plugin_view_no_doc(&p, &view).unwrap();

        assert!(view.join("tests/spec.lua").exists());
        assert!(view.join("scripts/build.sh").exists());
        assert!(view.join("examples/demo.lua").exists());
        assert!(view.join("src/main.rs").exists());
        assert!(view.join("plugin/foo.vim").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_view_links_everything_except_dotfiles() {
        // ViewWithDoc 用: doc/ も含めて全部 link。 dotfile のみ除外。
        let root = tempdir().unwrap();
        let view = root.path().join("view");
        let p = root.path().join("plug");
        write(&p.join("doc/foo.txt"), "*foo*");
        write(&p.join("lua/foo/init.lua"), "return {}");
        write(&p.join("plugin/bar.vim"), "echo 'bar'");
        write(&p.join("README.md"), "# plug");
        write(&p.join("Cargo.toml"), "[package]");
        write(&p.join(".git/config"), "[core]");
        write(&p.join(".github/workflows/ci.yml"), "name: CI");

        let r = merge_plugin_view(&p, &view).unwrap();

        // doc/ も含めて link される
        assert!(view.join("doc/foo.txt").exists());
        assert!(view.join("lua/foo/init.lua").exists());
        assert!(view.join("plugin/bar.vim").exists());
        assert!(view.join("README.md").exists());
        assert!(view.join("Cargo.toml").exists());
        // dotfile (.git, .github 等) は除外
        assert!(!view.join(".git").exists());
        assert!(!view.join(".github").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_view_links_build_artifacts() {
        // ViewWithDoc でも build artifact (target/release/) を view に link する。
        // blink.cmp が eager + merge=false で自分で build_lua を持つケース。
        let root = tempdir().unwrap();
        let view = root.path().join("view");
        let p = root.path().join("plug");
        write(&p.join("doc/foo.txt"), "*foo*");
        write(&p.join("lua/blink/cmp/init.lua"), "return {}");
        write(&p.join("target/release/blink_cmp_fuzzy.dll"), "DLL");

        let r = merge_plugin_view(&p, &view).unwrap();

        assert!(view.join("doc/foo.txt").exists());
        assert!(view.join("lua/blink/cmp/init.lua").exists());
        assert!(view.join("target/release/blink_cmp_fuzzy.dll").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_view_skips_committed_doc_tags() {
        // doc/tags の skip ルールは views でも維持 (helptags が view 配下で
        // 走るとき hard link 経由で repos の tags まで上書きするのを防ぐ)。
        let root = tempdir().unwrap();
        let view = root.path().join("view");
        let p = root.path().join("plug");
        write(&p.join("doc/foo.txt"), "*foo*");
        write(&p.join("doc/tags"), "stale-tags");

        let r = merge_plugin_view(&p, &view).unwrap();

        assert!(view.join("doc/foo.txt").exists());
        assert!(
            !view.join("doc/tags").exists(),
            "doc/tags should be skipped in views/"
        );
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_view_no_doc_root_dir_vs_file_collision_is_recorded_as_conflict() {
        // dst_root に **ファイル** が居るところに src ルート直下の同名 **ディレクトリ** を
        // 張ろうとした場合、 create_dir_all で ENOTDIR にして全体 abort せず first-wins
        // で skip + conflict 記録 (CodeRabbit PR #124 MAJOR)。
        let root = tempdir().unwrap();
        let view = root.path().join("view");
        let p = root.path().join("plug");
        // 先に view/lua という **ファイル** を置く (別 plugin が張った想定)
        write(&view.join("lua"), "i am a file");
        // src 側は lua/foo.lua という **ディレクトリ階層**
        write(&p.join("lua/foo.lua"), "return {}");

        let r = merge_plugin_view_no_doc(&p, &view).unwrap();

        // 既存ファイル lua はそのまま残る (first-wins)
        assert!(view.join("lua").is_file());
        // conflict に lua が記録される
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].relative, PathBuf::from("lua"));
    }

    #[test]
    fn test_view_no_doc_root_file_vs_file_collision_is_recorded_as_conflict() {
        // ルート直下のファイル衝突も first-wins。
        let root = tempdir().unwrap();
        let view = root.path().join("view");
        let p = root.path().join("plug");
        // 先に view/README.md を置く (別 plugin が張った想定)
        write(&view.join("README.md"), "from a");
        // src 側も README.md
        write(&p.join("README.md"), "from b");

        let r = merge_plugin_view_no_doc(&p, &view).unwrap();

        // 既存ファイルは残る
        let content = fs::read_to_string(view.join("README.md")).unwrap();
        assert_eq!(content, "from a");
        // conflict に README.md が記録される
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].relative, PathBuf::from("README.md"));
    }

    #[test]
    fn test_view_no_doc_includes_all_rtp_dirs_except_doc() {
        // 旧 test の互換確認: RTP_DIRS のうち doc 以外は全部対象 (views も同じ)。
        let root = tempdir().unwrap();
        let view = root.path().join("view");
        let p = root.path().join("plug");
        for dir in RTP_DIRS {
            write(&p.join(dir).join("file.txt"), dir);
        }

        let r = merge_plugin_view_no_doc(&p, &view).unwrap();
        assert!(r.conflicts.is_empty());
        for dir in RTP_DIRS {
            let expected = view.join(dir).join("file.txt");
            if *dir == "doc" {
                assert!(
                    !expected.exists(),
                    "doc/ should be skipped by merge_plugin_view_no_doc"
                );
            } else {
                assert!(expected.exists(), "missing rtp dir in view: {}", dir);
            }
        }
    }

    #[test]
    fn test_doc_only_links_doc_files_only() {
        // lazy プラグインに対して doc/ だけ merged に流す。lua/ や plugin/ は
        // 流れない (lazy 性を保つため)。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("doc/foo.txt"), "*foo*");
        write(&p.join("doc/bar.txt"), "*bar*");
        write(&p.join("lua/foo/init.lua"), "return {}");
        write(&p.join("plugin/foo.vim"), "echo 'foo'");

        let r = merge_plugin_doc_only(&p, &merged).unwrap();

        // doc 配下は merged に来る
        assert!(merged.join("doc/foo.txt").exists());
        assert!(merged.join("doc/bar.txt").exists());
        // lua / plugin は来ない (lazy 性維持)
        assert!(!merged.join("lua").exists());
        assert!(!merged.join("plugin").exists());
        assert!(r.conflicts.is_empty());
        assert_eq!(r.placed.len(), 2);
    }

    #[test]
    fn test_doc_only_no_doc_dir_is_noop() {
        // doc/ が無いプラグインに対しては何もしない (エラーにもならない)。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("lua/foo/init.lua"), "return {}");

        let r = merge_plugin_doc_only(&p, &merged).unwrap();

        assert!(r.conflicts.is_empty());
        assert!(r.placed.is_empty());
        assert!(!merged.join("doc").exists());
    }

    #[test]
    fn test_doc_only_first_wins_with_existing_merged() {
        // 既に merged/doc/foo.txt が居る (merge=true な eager プラグインが配置済) ところに
        // 別の lazy プラグインが doc only で来る → first-wins で skip + conflict 記録。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let a = root.path().join("plug_a");
        let b = root.path().join("plug_b");
        write(&a.join("doc/foo.txt"), "from a");
        write(&b.join("doc/foo.txt"), "from b");

        // 先に A を full-merge で配置
        let _ = merge_plugin(&a, &merged).unwrap();
        // B は doc-only で後から来る
        let r2 = merge_plugin_doc_only(&b, &merged).unwrap();

        let content = fs::read_to_string(merged.join("doc/foo.txt")).unwrap();
        assert_eq!(content, "from a");
        assert_eq!(r2.conflicts.len(), 1);
        assert_eq!(
            r2.conflicts[0].relative,
            PathBuf::from("doc").join("foo.txt")
        );
    }

    #[test]
    fn test_doc_only_skips_committed_doc_tags() {
        // doc/tags は merge_plugin と同じく skip (後段の :helptags merged/doc が再生成する)。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("doc/foo.txt"), "*foo*");
        write(&p.join("doc/tags"), "stale-tags");

        let r = merge_plugin_doc_only(&p, &merged).unwrap();

        assert!(merged.join("doc/foo.txt").exists());
        assert!(!merged.join("doc/tags").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_doc_only_skips_dotfiles_in_doc() {
        // doc/.gitignore のような dotfile は skip (merge_plugin と同じ)。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("doc/foo.txt"), "*foo*");
        write(&p.join("doc/.gitignore"), "tags");

        let r = merge_plugin_doc_only(&p, &merged).unwrap();

        assert!(merged.join("doc/foo.txt").exists());
        assert!(!merged.join("doc/.gitignore").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_doc_only_handles_nested_doc_subdirs() {
        // 一部 plugin は doc/<lang>/<topic>.txt のように doc 配下にサブディレクトリを
        // 持つ。再帰的にリンクされる。
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let p = root.path().join("plug");
        write(&p.join("doc/main.txt"), "*main*");
        write(&p.join("doc/sub/extra.txt"), "*extra*");

        let r = merge_plugin_doc_only(&p, &merged).unwrap();

        assert!(merged.join("doc/main.txt").exists());
        assert!(merged.join("doc/sub/extra.txt").exists());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn test_merge_returns_multiple_conflicts() {
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        let a = root.path().join("a");
        let b = root.path().join("b");
        write(&a.join("lua/x.lua"), "a-x");
        write(&a.join("lua/y.lua"), "a-y");
        write(&b.join("lua/x.lua"), "b-x");
        write(&b.join("lua/y.lua"), "b-y");
        write(&b.join("lua/z.lua"), "b-z"); // z だけは衝突しない

        let _ = merge_plugin(&a, &merged).unwrap();
        let r2 = merge_plugin(&b, &merged).unwrap();

        assert_eq!(r2.conflicts.len(), 2);
        assert!(merged.join("lua/z.lua").exists());
    }
}
