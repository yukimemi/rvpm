// プラグインの `plugin/`, `ftplugin/`, `after/plugin/`, `lua/` ディレクトリを
// 静的スキャンして、user-facing な hook 情報を集める:
//
//   - `commands`     : `nvim_create_user_command("Foo", ...)` / `command! Foo`
//   - `user_maps`    : `nnoremap gc ...` / `vim.keymap.set("n", "gc", ...)` 等、
//                       **`<Plug>(...)` LHS は除外**した「user が直接押すキー」
//   - `user_events`  : プラグインが `nvim_exec_autocmds("User", {pattern = "X"})`
//                       / `doautocmd User X` で fire する User event 名
//
// 用途:
//   - `on_cmd` の `/regex/` 展開 (#86, shipped) — `commands` を消費
//   - `on_map` の `/regex/` 展開 (#88) — `user_maps` を消費
//   - `rvpm add` 自動 lazy 提案 (#87 UI) — `commands` + `user_maps` を消費
//
// 制約:
//   - 動的定義 (computed name, setup() 内定義で setup 未呼出) は拾えない。
//     これらは user が exact 名を手書きする想定。
//   - `<Plug>(...)` LHS は **user-facing でない** ので user_maps から弾く。
//   - On-event suggestion は deadlock 的制約があり、プラグインが **発火する側** の
//     User event を自身の lazy trigger にはできない。user_events の収集は #88 の
//     regex 展開で user が他プラグインの event を trigger に書く際の参照用。

use regex::Regex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// user-facing キーマップ 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMap {
    pub lhs: String,
    pub modes: Vec<String>,
}

/// 1 プラグイン分のスキャン結果。各フィールドは順序保持 + dedup 済み (集約層)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanResult {
    pub commands: Vec<String>,
    pub user_maps: Vec<UserMap>,
    pub user_events: Vec<String>,
}

/// Lua / Vim-script のソース文字列から 3 種類の hook 情報を抽出する。
/// 出現順は保持、**重複除去は行わない** — 集約側 (`scan_files`) の責務。
pub fn scan_source(src: &str) -> ScanResult {
    let mut out = ScanResult::default();

    for line in src.lines() {
        // Lua `-- comment` は以降切り捨て (#86 review regression prevention)
        let code = line.find("--").map_or(line, |i| &line[..i]);

        scan_commands_in_line(code, &mut out.commands);
        scan_maps_in_line(line, code, &mut out.user_maps);
        scan_user_events_in_line(line, code, &mut out.user_events);
    }

    out
}

/// ファイルパスのリストを読み込んで集約 + dedup した ScanResult を返す。
pub fn scan_files<P: AsRef<Path>>(paths: &[P]) -> ScanResult {
    let mut commands_seen: HashSet<String> = HashSet::new();
    let mut maps_seen: HashSet<(String, Vec<String>)> = HashSet::new();
    let mut events_seen: HashSet<String> = HashSet::new();
    let mut agg = ScanResult::default();
    for p in paths {
        let Ok(src) = std::fs::read_to_string(p) else {
            continue;
        };
        let res = scan_source(&src);
        for c in res.commands {
            if commands_seen.insert(c.clone()) {
                agg.commands.push(c);
            }
        }
        for m in res.user_maps {
            let key = (m.lhs.clone(), m.modes.clone());
            if maps_seen.insert(key) {
                agg.user_maps.push(m);
            }
        }
        for e in res.user_events {
            if events_seen.insert(e.clone()) {
                agg.user_events.push(e);
            }
        }
    }
    agg
}

/// プラグイン root 配下のソースを走査。
/// 対象: `plugin/**`, `ftplugin/**`, `after/plugin/**`, `lua/**` の `.vim` / `.lua`。
///
/// `lua/` 追加により、modern plugin が setup() 内で `nvim_create_user_command` を
/// 定義する literal 定義を拾える (computed name は拾えない、制約として許容)。
pub fn scan_plugin(plugin_root: &Path) -> ScanResult {
    let mut files: Vec<PathBuf> = Vec::new();
    for sub in ["plugin", "ftplugin", "after/plugin", "lua"] {
        let dir = plugin_root.join(sub);
        if !dir.is_dir() {
            continue;
        }
        collect_scan_targets(&dir, &mut files);
    }
    scan_files(&files)
}

fn collect_scan_targets(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_scan_targets(&path, out);
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "vim" || ext == "lua" {
            out.push(path);
        }
    }
}

// ── command scanning ────────────────────────────────────────────────────

fn scan_commands_in_line(code: &str, out: &mut Vec<String>) {
    // Lua: vim.api.nvim_create_user_command("Foo", ...)
    if let Some(off) = code.find("nvim_create_user_command") {
        let rest = &code[off..];
        if let Some(open) = rest.find('(') {
            let after = rest[open + 1..].trim_start();
            if let Some(name) = extract_quoted_ident(after) {
                out.push(name);
            }
        }
    }
    // Vim script: `command! [-opts]* Foo ...` / `command [-opts]* Foo ...`
    let trimmed = code.trim_start();
    if let Some(after_cmd) = trimmed
        .strip_prefix("command!")
        .or_else(|| trimmed.strip_prefix("command "))
    {
        let mut rest = after_cmd.trim_start();
        while let Some(remaining) = rest.strip_prefix('-') {
            let end = remaining
                .find(char::is_whitespace)
                .unwrap_or(remaining.len());
            rest = remaining[end..].trim_start();
        }
        if let Some(name) = extract_ident(rest) {
            out.push(name);
        }
    }
}

// ── keymap scanning ─────────────────────────────────────────────────────

fn vim_map_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?P<prefix>[nvxiocstl]?)(?P<kind>noremap|map)(?P<bang>!?)\s+(?P<rest>.+)$")
            .unwrap()
    })
}

fn lua_map_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"vim\.(?:api\.nvim_set_keymap|keymap\.set)\s*\(\s*["'](?P<mode>[nvxiocstl])["']\s*,\s*["'](?P<lhs>[^"']+)["']"#,
        )
        .unwrap()
    })
}

fn scan_maps_in_line(full: &str, code: &str, out: &mut Vec<UserMap>) {
    // Vim: nnoremap <silent> <buffer> gc ...
    if let Some(caps) = vim_map_re().captures(full) {
        let prefix = caps.name("prefix").map_or("", |m| m.as_str());
        let bang = caps.name("bang").map_or("", |m| m.as_str());
        let rest = caps.name("rest").map_or("", |m| m.as_str());
        let modes = vim_map_modes(prefix, bang == "!");
        if let Some(lhs) = parse_vim_map_lhs(rest)
            && !is_plug_lhs(&lhs)
        {
            out.push(UserMap { lhs, modes });
        }
    }
    // Lua: vim.keymap.set("n", "gc", ...) / vim.api.nvim_set_keymap("n", "gc", ...)
    if let Some(caps) = lua_map_re().captures(code) {
        let mode = caps.name("mode").map_or("", |m| m.as_str());
        let lhs = caps.name("lhs").map_or("", |m| m.as_str());
        if !lhs.is_empty() && !is_plug_lhs(lhs) {
            out.push(UserMap {
                lhs: lhs.to_string(),
                modes: vec![mode.to_string()],
            });
        }
    }
}

fn parse_vim_map_lhs(rest: &str) -> Option<String> {
    let mut s = rest.trim_start();
    while let Some(after_lt) = s.strip_prefix('<') {
        let close = after_lt.find('>')?;
        let tag = &after_lt[..close];
        match tag.to_ascii_lowercase().as_str() {
            "silent" | "buffer" | "expr" | "nowait" | "unique" | "script" | "special" => {
                s = after_lt[close + 1..].trim_start();
            }
            _ => break,
        }
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    let lhs = s[..end].trim();
    if lhs.is_empty() {
        None
    } else {
        Some(lhs.to_string())
    }
}

fn vim_map_modes(prefix: &str, bang: bool) -> Vec<String> {
    if prefix.is_empty() {
        if bang {
            vec!["i".into(), "c".into()]
        } else {
            vec!["n".into(), "v".into(), "o".into()]
        }
    } else {
        vec![prefix.to_string()]
    }
}

fn is_plug_lhs(lhs: &str) -> bool {
    lhs.to_ascii_lowercase().starts_with("<plug>")
}

// ── User event scanning ─────────────────────────────────────────────────

fn lua_user_event_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"nvim_exec_autocmds\s*\(\s*["']User["']\s*,\s*\{[^}]*pattern\s*=\s*["'](?P<ev>[^"']+)["']"#,
        )
        .unwrap()
    })
}

fn vim_doautocmd_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*doautocmd(?:\s+<[^>]+>)*\s+User\s+(?P<ev>\S+)").unwrap())
}

fn scan_user_events_in_line(full: &str, code: &str, out: &mut Vec<String>) {
    if let Some(caps) = lua_user_event_re().captures(code) {
        out.push(caps["ev"].to_string());
    }
    if let Some(caps) = vim_doautocmd_re().captures(full) {
        out.push(caps["ev"].to_string());
    }
}

// ── shared ident helpers ────────────────────────────────────────────────

fn extract_quoted_ident(s: &str) -> Option<String> {
    let first = s.chars().next()?;
    if first != '"' && first != '\'' {
        return None;
    }
    let quote = first;
    let rest = &s[1..];
    let end = rest.find(quote)?;
    let name = &rest[..end];
    if is_valid_command_name(name) {
        Some(name.to_string())
    } else {
        None
    }
}

fn extract_ident(s: &str) -> Option<String> {
    let end = s
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(s.len());
    let name = &s[..end];
    if is_valid_command_name(name) {
        Some(name.to_string())
    } else {
        None
    }
}

fn is_valid_command_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── command scanning ───────────────────────────────────────────

    #[test]
    fn scan_source_picks_lua_nvim_create_user_command() {
        let src = r#"
vim.api.nvim_create_user_command("FooOne", function() end, { bang = true })
vim.api.nvim_create_user_command('FooTwo', function() end, {})
require('foo').bar("NotCmd")
"#;
        let mut out = scan_source(src).commands;
        out.sort();
        assert_eq!(out, vec!["FooOne", "FooTwo"]);
    }

    #[test]
    fn scan_source_picks_vim_command_bang_and_options() {
        let src = r#"
command! FooOne echo 'one'
command! -bang -nargs=* FooTwo echo 'two'
command -bar FooThree echo 'three'
command! -complete=file -nargs=1 FooFour echo 'four'
"#;
        let mut out = scan_source(src).commands;
        out.sort();
        assert_eq!(out, vec!["FooFour", "FooOne", "FooThree", "FooTwo"]);
    }

    #[test]
    fn scan_source_ignores_lua_comment_out_definitions() {
        let src = r#"
-- example: vim.api.nvim_create_user_command("Example", function() end)
vim.api.nvim_create_user_command("Real", function() end, {})
"#;
        assert_eq!(scan_source(src).commands, vec!["Real"]);
    }

    #[test]
    fn scan_source_preserves_command_duplicates() {
        let src = r#"
vim.api.nvim_create_user_command("Foo", function() end)
command! Foo echo 'same name'
"#;
        assert_eq!(scan_source(src).commands, vec!["Foo", "Foo"]);
    }

    // ── user-facing keymap scanning ────────────────────────────────

    #[test]
    fn scan_source_picks_vim_nnoremap_lhs() {
        let src = "nnoremap gc <Plug>(commentary)\nnnoremap gcc <Plug>(commentary-line)";
        let maps = scan_source(src).user_maps;
        assert_eq!(
            maps,
            vec![
                UserMap {
                    lhs: "gc".into(),
                    modes: vec!["n".into()]
                },
                UserMap {
                    lhs: "gcc".into(),
                    modes: vec!["n".into()]
                },
            ]
        );
    }

    #[test]
    fn scan_source_skips_silent_buffer_options() {
        let src = "nnoremap <silent> <buffer> gc :echo 'x'<CR>";
        let maps = scan_source(src).user_maps;
        assert_eq!(
            maps,
            vec![UserMap {
                lhs: "gc".into(),
                modes: vec!["n".into()]
            }]
        );
    }

    #[test]
    fn scan_source_filters_plug_lhs() {
        let src = "nnoremap <Plug>(Foo) :echo 'foo'<CR>\nnnoremap gc <Plug>(Bar)";
        let maps = scan_source(src).user_maps;
        assert_eq!(
            maps,
            vec![UserMap {
                lhs: "gc".into(),
                modes: vec!["n".into()]
            }]
        );
    }

    #[test]
    fn scan_source_extracts_mode_from_vim_prefix() {
        let src = "\
vnoremap gc <Plug>(comment)
inoremap gi <Plug>(i-cmd)
xnoremap gx <Plug>(visual)
cnoremap gc :echo 'cmdline'<CR>";
        let maps = scan_source(src).user_maps;
        assert_eq!(
            maps,
            vec![
                UserMap {
                    lhs: "gc".into(),
                    modes: vec!["v".into()]
                },
                UserMap {
                    lhs: "gi".into(),
                    modes: vec!["i".into()]
                },
                UserMap {
                    lhs: "gx".into(),
                    modes: vec!["x".into()]
                },
                UserMap {
                    lhs: "gc".into(),
                    modes: vec!["c".into()]
                },
            ]
        );
    }

    #[test]
    fn scan_source_bare_map_default_modes() {
        let src = "map gc <Plug>(Foo)";
        let maps = scan_source(src).user_maps;
        assert_eq!(
            maps,
            vec![UserMap {
                lhs: "gc".into(),
                modes: vec!["n".into(), "v".into(), "o".into()],
            }]
        );
    }

    #[test]
    fn scan_source_map_bang_is_insert_and_cmdline() {
        let src = "noremap! gc <Plug>(Foo)";
        let maps = scan_source(src).user_maps;
        assert_eq!(
            maps,
            vec![UserMap {
                lhs: "gc".into(),
                modes: vec!["i".into(), "c".into()],
            }]
        );
    }

    #[test]
    fn scan_source_picks_lua_keymap_set() {
        let src = r#"vim.keymap.set("n", "gc", function() end, {})"#;
        let maps = scan_source(src).user_maps;
        assert_eq!(
            maps,
            vec![UserMap {
                lhs: "gc".into(),
                modes: vec!["n".into()]
            }]
        );
    }

    #[test]
    fn scan_source_picks_lua_nvim_set_keymap() {
        let src = r#"vim.api.nvim_set_keymap("v", "gv", "<Plug>(Foo)", {})"#;
        let maps = scan_source(src).user_maps;
        assert_eq!(
            maps,
            vec![UserMap {
                lhs: "gv".into(),
                modes: vec!["v".into()]
            }]
        );
    }

    #[test]
    fn scan_source_filters_lua_plug_lhs() {
        let src = r#"vim.keymap.set("n", "<Plug>(Internal)", function() end)"#;
        assert!(scan_source(src).user_maps.is_empty());
    }

    // ── User event scanning ────────────────────────────────────────

    #[test]
    fn scan_source_picks_lua_user_event_pattern() {
        let src = r#"vim.api.nvim_exec_autocmds("User", { pattern = "FooDone" })"#;
        assert_eq!(scan_source(src).user_events, vec!["FooDone"]);
    }

    #[test]
    fn scan_source_picks_vim_doautocmd_user() {
        let src = "doautocmd User BarReady";
        assert_eq!(scan_source(src).user_events, vec!["BarReady"]);
    }

    #[test]
    fn scan_source_picks_vim_doautocmd_with_options() {
        let src = "doautocmd <nomodeline> User BarReady";
        assert_eq!(scan_source(src).user_events, vec!["BarReady"]);
    }

    // ── file / plugin aggregation ──────────────────────────────────

    #[test]
    fn scan_files_dedups_across_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.lua");
        let b = tmp.path().join("b.vim");
        std::fs::write(
            &a,
            "vim.api.nvim_create_user_command('Foo', function() end)\n\
             vim.api.nvim_create_user_command('Foo', function() end)",
        )
        .unwrap();
        std::fs::write(&b, "command! Foo echo 'b'\nnnoremap gc <Plug>(c)").unwrap();
        let result = scan_files(&[a, b]);
        assert_eq!(result.commands, vec!["Foo"]);
        assert_eq!(
            result.user_maps,
            vec![UserMap {
                lhs: "gc".into(),
                modes: vec!["n".into()]
            }]
        );
    }

    #[test]
    fn scan_plugin_walks_plugin_ftplugin_after_and_lua() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (sub, fname, body) in [
            (
                "plugin",
                "a.lua",
                "vim.api.nvim_create_user_command('PluginA', function() end, {})",
            ),
            ("ftplugin", "rust.vim", "command! FtRust echo 'rust'"),
            (
                "after/plugin",
                "b.vim",
                "command! -bang AfterB echo 'after'",
            ),
            (
                // modern plugin: setup() 内 literal 定義 (lua/ 追加の効果)
                "lua/foo",
                "init.lua",
                r#"return { setup = function() vim.api.nvim_create_user_command("Setupd", function() end, {}) end }"#,
            ),
            // scan 対象外ディレクトリ
            ("autoload", "x.vim", "command! NotScanned echo 'no'"),
        ] {
            let dir = root.join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(fname), body).unwrap();
        }
        let mut out = scan_plugin(root).commands;
        out.sort();
        assert_eq!(out, vec!["AfterB", "FtRust", "PluginA", "Setupd"]);
    }
}
