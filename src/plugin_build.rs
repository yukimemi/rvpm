//! Plugin build-step execution + loader script pre-glob (#217).
//!
//! Two related build-time concerns extracted verbatim from the monolithic
//! lib.rs:
//! - Running a plugin's `build` / `build_lua` step after sync / update
//!   (`execute_build_command` and the shell / lua / rtp helpers).
//! - Pre-globbing each plugin's `plugin/` `ftdetect/` `colors/` `denops/` …
//!   into a [`crate::loader::PluginScripts`] at generate time
//!   (`build_plugin_scripts` + the `collect_*` helpers, `parse_build_command`).

use crate::PluginMergeMode;
use std::path::{Path, PathBuf};

/// プラグインの build ステップを実行する。`build` (shell) と `build_lua` の両方が
/// 設定されていれば、shell → lua の順に実行する。どちらか一方でも失敗したら
/// 即座にエラー文字列を返す。両方未設定なら `None`。
pub(crate) async fn execute_build_command(
    plugin: &crate::config::Plugin,
    dst_path: &Path,
    config: &crate::config::Config,
    cache_root: &Path,
) -> Option<String> {
    // 早期 return: build step が一つも無ければ rtp 計算自体スキップ (Gemini #99
    // 指摘の short-circuit。大きな depends グラフでの無駄走査を避ける)。
    if plugin.build.is_none() && plugin.build_lua.is_none() {
        return None;
    }

    let rtp_dirs = collect_build_rtp(plugin, dst_path, config, cache_root);

    // 1. shell コマンド (従来挙動)。失敗時は即 return。
    if let Some(err) = run_build_shell(plugin, dst_path, &rtp_dirs).await {
        return Some(err);
    }

    // 2. Lua callable (#97)。例: blink.cmp v2 の `require('blink.cmp').build():wait(...)`。
    if let Some(err) = run_build_lua(plugin, dst_path, &rtp_dirs).await {
        return Some(err);
    }

    None
}

/// build 実行時の rtp 候補一覧 (対象プラグイン + transitive depends パス)。
/// 既存の shell build もこれを使ってきたので、Lua build 側も同じ rtp で揃える。
pub(crate) fn collect_build_rtp(
    plugin: &crate::config::Plugin,
    dst_path: &Path,
    config: &crate::config::Config,
    cache_root: &Path,
) -> Vec<PathBuf> {
    let mut rtp_dirs = vec![dst_path.to_path_buf()];
    let mut visited = std::collections::HashSet::new();
    let mut stack: Vec<String> = plugin.depends.iter().flatten().cloned().collect();
    while let Some(dep) = stack.pop() {
        if !visited.insert(dep.clone()) {
            continue;
        }
        if let Some(dep_plugin) = config
            .plugins
            .iter()
            .find(|p| p.display_name() == dep || p.url == dep)
        {
            let dep_path = crate::resolve_plugin_dst(dep_plugin, cache_root);
            rtp_dirs.push(dep_path);
            if let Some(deeper) = &dep_plugin.depends {
                stack.extend(deeper.clone());
            }
        }
    }
    rtp_dirs
}

/// shell `build` を実行する。未設定 / 成功なら `None`、失敗なら error message。
pub(crate) async fn run_build_shell(
    plugin: &crate::config::Plugin,
    dst_path: &Path,
    rtp_dirs: &[PathBuf],
) -> Option<String> {
    let build_cmd = plugin.build.as_ref()?;
    let (prog, args) = parse_build_command(build_cmd, rtp_dirs);
    let build_timeout = std::time::Duration::from_secs(300); // 5 minutes
    let child = match tokio::process::Command::new(&prog)
        .args(&args)
        .current_dir(dst_path)
        // Capture stdout/stderr and drain them via `wait_with_output()`: leaving
        // piped output unread deadlocks the child once it writes past the OS
        // pipe buffer (~64 KB), e.g. a chatty `cargo build` / `:TSUpdate` (#226).
        // Draining also lets us surface the build's stderr on failure instead of
        // a bare exit code. `kill_on_drop(true)` kills a timed-out build when the
        // future is dropped (mirrors `run_build_lua`).
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Some(format!("build spawn failed: {}", e));
        }
    };
    match tokio::time::timeout(build_timeout, child.wait_with_output()).await {
        Ok(Ok(out)) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Some(format!(
                "build failed (exit code: {:?}): {}",
                out.status.code(),
                stderr.trim()
            ))
        }
        Ok(Ok(_)) => None,
        Ok(Err(e)) => Some(format!("build error: {}", e)),
        Err(_) => Some(format!("build timed out ({}s)", build_timeout.as_secs())),
    }
}

/// `build_lua` に **lazy.nvim 流の `function() ... end` で囲まれた匿名関数式** が
/// 渡された場合、その body だけを取り出して返す。
///
/// なぜ必要か: rvpm は `build_lua` の文字列を **statement context の Lua スニペット**
/// として exec する (`-l <tmp.lua>`)。素の `function() ... end` は statement では
/// 「function NAME() ... end」と解釈されるので名前必須 (E5112) になる。
/// user は lazy.nvim の `build = function() require(...).build():wait(...) end` から
/// 流用しがちなので、その形を検出して body へ unwrap する。
///
/// **対応する記述**:
/// - 1 行 / 複数行どちらも (regex `(?s)` で `.` が改行マッチ)
/// - `function()` 直後・`end` 直前の任意の空白/改行
/// - `end` の後ろの trailing line コメント (`-- ...`) と空白
/// - 入れ子の `function() ... end` は body 内に **そのまま保持** (lazy quantifier
///   `.*?` + 末尾 anchor `$` で「最後の一番外の `end`」を拾うため)
///
/// **マッチしない**:
/// - `function name() ... end` (named function 宣言は valid statement なので unwrap 不要)
/// - 末尾に statement や `;` が続くケース (lazy.nvim 流ではない)
pub(crate) fn unwrap_anonymous_lua_function(code: &str) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // (?s): . が \n にマッチする dot-all モード。
        // (.*?): lazy match で末尾 anchor `$` と組み合わせて「最後の `end`」を拾う。
        // 末尾の trailing line コメント (`-- ...`) は optional。
        Regex::new(r"(?s)^\s*function\s*\(\s*\)\s*(.*?)\s*end\s*(?:--[^\n\r]*)?\s*$").unwrap()
    });
    re.captures(code)
        .map(|c| c.get(1).map_or("", |m| m.as_str()).trim().to_string())
}

/// Lua callable `build_lua` を実行する (#97)。
///
/// `nvim --headless -u NONE -l <tmp.lua>` で起動し、rtp に対象プラグイン +
/// transitive deps を append した上で user の Lua スニペットを呼ぶ。
///
/// `-u NONE` で user init.lua はスキップするが、`vim.fn.stdpath("data")` 等の
/// real env は維持されるので blink.cmp v2 のように `~/.local/share/nvim/site/lib/`
/// 等にファイルを置きたいケースでも期待通りの場所に届く。
///
/// nvim が PATH に無ければ warn + skip (resilience、helptags と同じ方針)。
pub(crate) async fn run_build_lua(
    plugin: &crate::config::Plugin,
    dst_path: &Path,
    rtp_dirs: &[PathBuf],
) -> Option<String> {
    let raw_lua_code = plugin.build_lua.as_ref()?;

    // user は lazy.nvim の `build = function() ... end` 流れで
    // `build_lua = "function() ... end"` と書きがちだが、Lua の statement context
    // で `function()` (匿名) は名前必須なので E5112 エラーになる。
    // → `function() X end` の wrapping を検出したら body だけを残す。
    let lua_code = unwrap_anonymous_lua_function(raw_lua_code)
        .map(std::borrow::Cow::Owned)
        .unwrap_or(std::borrow::Cow::Borrowed(raw_lua_code.as_str()));

    // rtp_dirs を `vim.opt.rtp:append(...)` の連続で前置する。生のパスを Lua
    // 文字列リテラルに埋め込むので、backslash は `/` に正規化、引用符は escape。
    let mut script = String::new();
    for dir in rtp_dirs {
        let p = dir
            .to_string_lossy()
            .replace('\\', "/")
            .replace('"', "\\\"");
        script.push_str(&format!("vim.opt.rtp:append(\"{p}\")\n"));
    }
    script.push_str(&lua_code);
    script.push('\n');

    // tempfile crate に temp ファイル管理を委ねる (Gemini #99 指摘): manual な
    // SystemTime + process::id 組立ては、process が中断された場合にゴミが残る。
    // `NamedTempFile` は drop で自動削除 + プラットフォームに合った安全な命名を行う。
    let tmp_file = match tempfile::Builder::new()
        .prefix("rvpm-build-")
        .suffix(".lua")
        .tempfile()
    {
        Ok(f) => f,
        Err(e) => return Some(format!("build_lua: failed to create temp script: {e}")),
    };
    let tmp_path = tmp_file.path().to_path_buf();
    // async 経路なので blocking な std::fs::write は tokio::fs::write に置換
    // (Gemini #99 review): executor を塞いで他プラグインの並列 build を妨げないように。
    if let Err(e) = tokio::fs::write(&tmp_path, &script).await {
        return Some(format!("build_lua: failed to write temp script: {e}"));
    }

    let build_timeout = std::time::Duration::from_secs(300);
    let child = match tokio::process::Command::new("nvim")
        .args(["--headless", "-u", "NONE", "-l"])
        .arg(&tmp_path)
        .current_dir(dst_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // tokio Command の default は `kill_on_drop = false` なので、timeout 時に
        // future が drop されても child の nvim は走り続けてしまう (Gemini High
        // 指摘の resource leak)。`true` で drop = kill 連動させる。
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Some(format!(
                "build_lua: failed to spawn `nvim --headless` ({e}); install nvim or remove `build_lua` from this plugin"
            ));
        }
    };

    let wait_result = tokio::time::timeout(build_timeout, child.wait_with_output()).await;
    // tmp_file は scope を抜ける時 drop で自動削除されるので明示削除は不要。
    drop(tmp_file);

    match wait_result {
        Ok(Ok(out)) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Some(format!(
                "build_lua failed (exit code: {:?}): {}",
                out.status.code(),
                stderr.trim()
            ))
        }
        Ok(Ok(_)) => None,
        Ok(Err(e)) => Some(format!("build_lua error: {e}")),
        Err(_) => Some(format!(
            "build_lua timed out ({}s)",
            build_timeout.as_secs()
        )),
    }
}

/// 指定ディレクトリ配下を再帰的に walk し、`.vim` / `.lua` ファイルをソートして返す。
/// lazy.nvim の Util.walk + source_runtime のフィルタと同等。
/// ディレクトリが存在しない場合は空配列を返す (Resilience)。
/// `colors/` ディレクトリからカラースキーム名 (ファイル名から拡張子を除去) を収集する。
/// 例: `colors/catppuccin.lua` → `"catppuccin"`, `colors/catppuccin-latte.vim` → `"catppuccin-latte"`
/// build コマンドを解析して (実行プログラム, 引数リスト) を返す。
/// `:` で始まる場合は Neovim コマンドとして実行。rtp_dirs (自身 + 依存先) を
/// rtp に追加してコマンドや autoload 関数を使えるようにする。
/// それ以外はシェルコマンドとして `sh -c "..."` (Windows: `cmd /C "..."`) に変換。
pub(crate) fn parse_build_command(build_cmd: &str, rtp_dirs: &[PathBuf]) -> (String, Vec<String>) {
    if let Some(vim_cmd) = build_cmd.strip_prefix(':') {
        let rtp_cmds: Vec<String> = rtp_dirs
            .iter()
            .map(|d| format!("set rtp+={}", d.to_string_lossy().replace('\\', "/")))
            .collect();
        let rtp_cmd = rtp_cmds.join(" | ");
        (
            "nvim".to_string(),
            vec![
                "--headless".to_string(),
                "--cmd".to_string(),
                rtp_cmd,
                "-c".to_string(),
                vim_cmd.to_string(),
                "-c".to_string(),
                "qa!".to_string(),
            ],
        )
    } else if cfg!(windows) {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), build_cmd.to_string()],
        )
    } else {
        (
            "sh".to_string(),
            vec!["-c".to_string(), build_cmd.to_string()],
        )
    }
}

pub(crate) fn collect_colorschemes(plugin_path: &Path) -> Vec<String> {
    let dir = plugin_path.join("colors");
    if !dir.exists() {
        return Vec::new();
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let path = e.path();
            let ext = path.extension()?.to_str()?;
            if ext == "lua" || ext == "vim" {
                Some(path.file_stem()?.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// `<plugin_path>/denops/<name>/main.{ts,js}` を走査して denops プラグイン情報を返す。
/// denops.vim は main.ts を優先的に discover するため、存在すれば main.ts、
/// なければ main.js を採用する (どちらも無ければ対象外)。
pub(crate) fn collect_denops_plugins(plugin_path: &Path) -> Vec<crate::loader::DenopsPlugin> {
    let dir = plugin_path.join("denops");
    if !dir.exists() {
        return Vec::new();
    }
    let mut plugins: Vec<crate::loader::DenopsPlugin> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        // `Path::is_dir()` は symlink を follow するので、dev plugin や
        // mono-repo で symlink された `denops/<name>/` も拾える。
        // `DirEntry::file_type()` だと symlink 自体を見てしまい skip される。
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let sub = e.path();
            let name = sub.file_name()?.to_string_lossy().to_string();
            for candidate in ["main.ts", "main.js"] {
                let script = sub.join(candidate);
                if script.is_file() {
                    return Some(crate::loader::DenopsPlugin {
                        name,
                        main_script: script.to_string_lossy().replace('\\', "/"),
                    });
                }
            }
            None
        })
        .collect();
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    plugins
}

pub(crate) fn collect_source_files(plugin_path: &Path, subdir: &str) -> Vec<String> {
    let dir = plugin_path.join(subdir);
    if !dir.exists() {
        return Vec::new();
    }
    let mut files: Vec<String> = walkdir::WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext == "lua" || ext == "vim")
                .unwrap_or(false)
        })
        .map(|e| e.path().to_string_lossy().replace('\\', "/"))
        .collect();
    files.sort();
    files
}

/// Plugin の実ディスク情報から PluginScripts を構築するヘルパー。
/// run_sync / run_generate で重複していたロジックを集約。
///
/// `view_path` は `views/<host>/<owner>/<repo>/` (#119)。 Full merge 対象 (eager+merge=true)
/// では loader 側で `merged/` を rtp:append するのでこの値は使われないが、 後段で
/// MergeMode を見て分岐するためにレコードに保持しておく。
pub(crate) fn build_plugin_scripts(
    plugin: &crate::config::Plugin,
    plugin_path: &Path,
    plugin_config_dir: &Path,
    view_path: &Path,
    mode: PluginMergeMode,
) -> crate::loader::PluginScripts {
    // on_cmd / on_map / on_event の /regex/ 展開用に 1 回だけ静的スキャン (#85, #88)。
    // 対象は plugin/, ftplugin/, after/plugin/, lua/ 配下の .vim / .lua のみで、
    // load 経路に影響しないので dead plugin でもコストは小さい。
    let scan = crate::plugin_scan::scan_plugin(plugin_path);
    let uses_merged = matches!(
        mode,
        PluginMergeMode::Full | PluginMergeMode::ViewWithoutDoc
    );
    crate::loader::PluginScripts {
        name: plugin.display_name(),
        path: plugin_path.to_string_lossy().replace('\\', "/"),
        view_path: view_path.to_string_lossy().replace('\\', "/"),
        merge: plugin.merge,
        merge_doc: plugin.merge_doc,
        uses_merged,
        init: crate::find_lua(plugin_config_dir, "init.lua"),
        before: crate::find_lua(plugin_config_dir, "before.lua"),
        after: crate::find_lua(plugin_config_dir, "after.lua"),
        plugin_files: collect_source_files(plugin_path, "plugin"),
        ftdetect_files: collect_source_files(plugin_path, "ftdetect"),
        after_plugin_files: collect_source_files(plugin_path, "after/plugin"),
        lazy: plugin.lazy,
        on_cmd: plugin.on_cmd.clone(),
        on_ft: plugin.on_ft.clone(),
        on_map: plugin.on_map.clone(),
        on_event: plugin.on_event.clone(),
        on_path: plugin.on_path.clone(),
        on_source: plugin.on_source.clone(),
        depends: plugin.depends.clone(),
        colorschemes: collect_colorschemes(plugin_path),
        denops_plugins: collect_denops_plugins(plugin_path),
        defined_commands: scan.commands,
        defined_plug_maps: scan.plug_maps,
        defined_user_events: scan.user_events,
        cond: plugin.cond.clone(),
        dev: plugin.dev,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Options, Plugin};
    use tempfile::tempdir;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn test_collect_denops_plugins_finds_main_ts() {
        let root = tempdir().unwrap();
        let plugin = root.path().join("plugin-repo");
        write_file(
            &plugin.join("denops/foo/main.ts"),
            "export async function main() {}",
        );
        write_file(&plugin.join("denops/foo/util.ts"), "export const x = 1;");

        let got = collect_denops_plugins(&plugin);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "foo");
        assert!(
            got[0].main_script.ends_with("denops/foo/main.ts"),
            "main_script should be absolute path ending with denops/foo/main.ts, got: {}",
            got[0].main_script
        );
        // forward slash に正規化されている
        assert!(
            !got[0].main_script.contains('\\'),
            "main_script must use forward slashes"
        );
    }

    #[test]
    fn test_collect_denops_plugins_returns_empty_without_denops_dir() {
        let root = tempdir().unwrap();
        let plugin = root.path().join("plugin-repo");
        write_file(&plugin.join("plugin/foo.vim"), "echo 'foo'");
        // denops/ が無い
        let got = collect_denops_plugins(&plugin);
        assert!(got.is_empty());
    }

    #[test]
    fn test_collect_denops_plugins_falls_back_to_main_js() {
        let root = tempdir().unwrap();
        let plugin = root.path().join("plugin-repo");
        // main.ts なし、main.js のみ
        write_file(
            &plugin.join("denops/bar/main.js"),
            "export async function main() {}",
        );

        let got = collect_denops_plugins(&plugin);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "bar");
        assert!(got[0].main_script.ends_with("denops/bar/main.js"));
    }

    #[test]
    fn test_collect_denops_plugins_prefers_main_ts_over_main_js() {
        let root = tempdir().unwrap();
        let plugin = root.path().join("plugin-repo");
        write_file(&plugin.join("denops/dual/main.ts"), "ts");
        write_file(&plugin.join("denops/dual/main.js"), "js");
        let got = collect_denops_plugins(&plugin);
        assert_eq!(got.len(), 1);
        assert!(got[0].main_script.ends_with("main.ts"));
    }

    #[test]
    fn test_collect_denops_plugins_skips_dirs_without_main() {
        let root = tempdir().unwrap();
        let plugin = root.path().join("plugin-repo");
        // main.ts も main.js も無いディレクトリは無視
        write_file(&plugin.join("denops/incomplete/other.ts"), "");
        write_file(&plugin.join("denops/ok/main.ts"), "");
        let got = collect_denops_plugins(&plugin);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "ok");
    }

    #[cfg(unix)]
    #[test]
    fn test_collect_denops_plugins_follows_symlinked_subdir() {
        // dev plugin や mono-repo の構成で、denops/<name>/ が別ディレクトリへの
        // symlink になっているケースを follow して検出する。
        // Windows は symlink 作成に管理者権限が要るので Unix のみで実行。
        let root = tempdir().unwrap();
        let plugin = root.path().join("plugin-repo");
        let real = root.path().join("external").join("real-denops");
        write_file(&real.join("main.ts"), "export async function main() {}");
        std::fs::create_dir_all(plugin.join("denops")).unwrap();
        std::os::unix::fs::symlink(&real, plugin.join("denops/sym-linked")).unwrap();

        let got = collect_denops_plugins(&plugin);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "sym-linked");
        assert!(got[0].main_script.ends_with("main.ts"));
    }

    #[test]
    fn test_collect_denops_plugins_multiple_sorted_by_name() {
        // 同一プラグイン repo が複数の denops サブモジュールを持つケース
        // (例: 一部の mono-repo) も決定論的な順序を保証
        let root = tempdir().unwrap();
        let plugin = root.path().join("plugin-repo");
        write_file(&plugin.join("denops/zeta/main.ts"), "");
        write_file(&plugin.join("denops/alpha/main.ts"), "");
        write_file(&plugin.join("denops/mid/main.ts"), "");
        let got = collect_denops_plugins(&plugin);
        let names: Vec<&str> = got.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn test_parse_build_command_shell() {
        let dirs = vec![PathBuf::from("/path/to/plugin")];
        let (cmd, args) = parse_build_command("cargo build --release", &dirs);
        if cfg!(windows) {
            assert_eq!(cmd, "cmd");
            assert_eq!(args, vec!["/C", "cargo build --release"]);
        } else {
            assert_eq!(cmd, "sh");
            assert_eq!(args, vec!["-c", "cargo build --release"]);
        }
    }

    #[test]
    fn test_parse_build_command_vim_prefix() {
        let dirs = vec![PathBuf::from("/path/to/plugin")];
        let (cmd, args) = parse_build_command(":call mkdp#util#install()", &dirs);
        assert_eq!(cmd, "nvim");
        assert!(args.iter().any(|a| a == "--headless"));
        assert!(args.iter().any(|a| a.contains("mkdp#util#install()")));
    }

    #[test]
    fn test_parse_build_command_vim_simple() {
        let dirs = vec![PathBuf::from("/path/to/plugin")];
        let (cmd, args) = parse_build_command(":TSUpdate", &dirs);
        assert_eq!(cmd, "nvim");
        assert!(args.iter().any(|a| a == "--headless"));
        assert!(args.iter().any(|a| a.contains("TSUpdate")));
    }

    #[test]
    fn test_parse_build_command_vim_adds_rtp() {
        let dirs = vec![PathBuf::from("/path/to/my-plugin")];
        let (cmd, args) = parse_build_command(":MyBuild", &dirs);
        assert_eq!(cmd, "nvim");
        assert!(args.iter().any(|a| a == "--cmd"));
        assert!(
            args.iter()
                .any(|a| a.contains("set rtp+=/path/to/my-plugin")),
            "should add plugin dir to rtp: {:?}",
            args
        );
    }

    #[test]
    fn test_parse_build_command_vim_includes_deps_rtp() {
        let dirs = vec![
            PathBuf::from("/path/to/plugin"),
            PathBuf::from("/path/to/dep1"),
            PathBuf::from("/path/to/dep2"),
        ];
        let (cmd, args) = parse_build_command(":Build", &dirs);
        assert_eq!(cmd, "nvim");
        let rtp_arg = args
            .iter()
            .find(|a| a.contains("set rtp+="))
            .expect("should have rtp cmd");
        assert!(rtp_arg.contains("/path/to/plugin"), "self: {}", rtp_arg);
        assert!(rtp_arg.contains("/path/to/dep1"), "dep1: {}", rtp_arg);
        assert!(rtp_arg.contains("/path/to/dep2"), "dep2: {}", rtp_arg);
    }

    // ─── build_lua / collect_build_rtp (#97) ────────────────────────────

    #[test]
    fn collect_build_rtp_includes_self_first_then_transitive_deps() {
        // A depends [B], B depends [C]. rtp should be [A, B, C]. (#97)
        let plugin_a = Plugin {
            url: "owner/a".to_string(),
            depends: Some(vec!["b".to_string()]),
            ..Default::default()
        };
        let plugin_b = Plugin {
            name: Some("b".to_string()),
            url: "owner/b".to_string(),
            depends: Some(vec!["c".to_string()]),
            ..Default::default()
        };
        let plugin_c = Plugin {
            name: Some("c".to_string()),
            url: "owner/c".to_string(),
            ..Default::default()
        };
        let config = Config {
            vars: None,
            options: Options::default(),
            plugins: vec![plugin_a.clone(), plugin_b, plugin_c],
        };
        let cache_root = PathBuf::from("/cache");
        let rtp = collect_build_rtp(
            &plugin_a,
            &PathBuf::from("/cache/plugins/repos/owner/a"),
            &config,
            &cache_root,
        );
        // self が先頭
        assert_eq!(rtp[0], PathBuf::from("/cache/plugins/repos/owner/a"));
        // transitive deps が含まれる (順序は DFS なので厳密には保証しないが、
        // 全部存在することを確認)
        let rtp_strings: Vec<String> = rtp.iter().map(|p| p.display().to_string()).collect();
        assert!(
            rtp_strings.iter().any(|s| s.contains("owner/b")),
            "B in rtp: {rtp_strings:?}"
        );
        assert!(
            rtp_strings.iter().any(|s| s.contains("owner/c")),
            "C in rtp: {rtp_strings:?}"
        );
    }

    #[test]
    fn unwrap_anonymous_lua_function_strips_lazy_nvim_style_wrapper() {
        // lazy.nvim style — user が `build = function() ... end` から copy
        let r = unwrap_anonymous_lua_function(
            "function() require('blink.cmp').build():wait(60000) end",
        );
        assert_eq!(
            r.as_deref(),
            Some("require('blink.cmp').build():wait(60000)"),
            "function() body end → body extraction"
        );
    }

    #[test]
    fn unwrap_anonymous_lua_function_handles_space_after_function_keyword() {
        let r = unwrap_anonymous_lua_function("function ()  vim.cmd('TSUpdate')  end");
        assert_eq!(r.as_deref(), Some("vim.cmd('TSUpdate')"));
    }

    #[test]
    fn unwrap_anonymous_lua_function_returns_none_for_plain_statement() {
        // 既に statement なら触らない (None で「unwrap 不要」と通知)。
        let r = unwrap_anonymous_lua_function("require('x').build():wait(60000)");
        assert!(r.is_none());
    }

    #[test]
    fn unwrap_anonymous_lua_function_returns_none_for_named_function_decl() {
        // `function name() ... end` は valid な statement (function 宣言) なので
        // 触る理由が無い。
        let r = unwrap_anonymous_lua_function("function build_step() require('x') end");
        assert!(r.is_none());
    }

    #[test]
    fn unwrap_anonymous_lua_function_does_not_split_identifier_ending_in_end() {
        // `local end_var = 1 end` のように identifier が "end" で終わるケースで
        // strip_suffix が誤動作しないこと。strip_suffix は exact suffix match
        // なので、"end_var" の "end" 部分は拾わない (前後の文字を見るので)。
        let r = unwrap_anonymous_lua_function("function() local end_var = 1 end");
        assert_eq!(r.as_deref(), Some("local end_var = 1"));
    }

    #[test]
    fn unwrap_anonymous_lua_function_preserves_inner_function_blocks() {
        // 入れ子の `function() ... end` は inner 側を保持する (lazy `.*?` + 末尾
        // anchor で「最後の一番外の end」だけを落とす)。
        let r =
            unwrap_anonymous_lua_function("function() local f = function() return 1 end; f() end");
        assert_eq!(r.as_deref(), Some("local f = function() return 1 end; f()"));
    }

    #[test]
    fn unwrap_anonymous_lua_function_handles_multiline_body() {
        // 改行を含む typical な複数行 build_lua。
        let code = "function()\n  local cmp = require('blink.cmp')\n  cmp.build():wait(60000)\n  vim.print('done')\nend";
        let r = unwrap_anonymous_lua_function(code);
        let body = r.expect("multiline function() should unwrap");
        assert!(body.contains("local cmp = require('blink.cmp')"));
        assert!(body.contains("cmp.build():wait(60000)"));
        assert!(body.contains("vim.print('done')"));
        assert!(!body.contains("function()"));
        // 末尾に余分な end が残ってないこと
        assert!(!body.ends_with("end"));
    }

    #[test]
    fn unwrap_anonymous_lua_function_handles_complex_multiline_with_nested_function() {
        // 複雑な複数行: ローカル関数定義入り。inner の function() ... end は保持。
        let code = "function()\n  local helper = function(x)\n    return x * 2\n  end\n  print(helper(21))\nend";
        let r = unwrap_anonymous_lua_function(code);
        let body = r.expect("complex multiline should unwrap outer only");
        assert!(body.contains("local helper = function(x)"));
        assert!(body.contains("return x * 2"));
        assert!(body.contains("print(helper(21))"));
        // inner の helper 定義 end が残ってる
        assert!(
            body.matches("end").count() >= 1,
            "inner helper end should remain, body: {body}"
        );
    }

    #[test]
    fn unwrap_anonymous_lua_function_tolerates_trailing_line_comment() {
        // `end` の後に行コメントが付くケース (regex で末尾 `(?:--[^\n]*)?` 許容)。
        let r = unwrap_anonymous_lua_function("function() vim.print('hi') end -- post-build hook");
        assert_eq!(r.as_deref(), Some("vim.print('hi')"));
    }

    #[test]
    fn unwrap_anonymous_lua_function_tolerates_whitespace_inside_parens() {
        // `function ( )` のような余分な空白も許容。
        let r = unwrap_anonymous_lua_function("function(  )  print('x')  end");
        assert_eq!(r.as_deref(), Some("print('x')"));
    }

    #[test]
    fn unwrap_anonymous_lua_function_returns_none_when_function_takes_args() {
        // `function(a, b)` のように引数があるものは触らない (匿名 zero-arity 専用)。
        let r = unwrap_anonymous_lua_function("function(a, b) return a + b end");
        assert!(r.is_none(), "function with args should not be unwrapped");
    }

    #[tokio::test]
    async fn run_build_lua_returns_none_when_field_unset() {
        let plugin = Plugin {
            url: "x/y".to_string(),
            ..Default::default()
        };
        let result = run_build_lua(&plugin, &PathBuf::from("/tmp"), &[]).await;
        assert!(
            result.is_none(),
            "no build_lua field → no-op, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn run_build_lua_reports_error_when_nvim_missing() {
        // nvim が PATH に無くても rvpm はクラッシュさせず、明示的なエラー文字列を返す。
        // この test は nvim がインストールされてる環境ではスキップされる
        // (nvim コマンドが本当に成功してしまうため)。CI / 開発機の typical 構成では
        // nvim あり、ローカル run_build_lua の no-build-lua path を検証する別 test
        // (run_build_lua_returns_none_when_field_unset) で十分。
        if which::which("nvim").is_ok() {
            return;
        }
        let tmp = tempdir().unwrap();
        let plugin = Plugin {
            url: "x/y".to_string(),
            build_lua: Some("vim.print('hi')".to_string()),
            ..Default::default()
        };
        let result = run_build_lua(&plugin, tmp.path(), &[tmp.path().to_path_buf()]).await;
        let err = result.expect("missing nvim should yield an error");
        assert!(
            err.contains("failed to spawn") || err.contains("nvim"),
            "error should mention nvim: {err}"
        );
    }
}
