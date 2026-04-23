// Vim / Lua source からプラグインが定義するユーザコマンド名を抽出する。
//
// 用途:
//   - `on_cmd` の `/regex/` エントリ展開 (#85) — rvpm generate 時に静的に
//     展開して loader.lua に焼き込む。runtime コスト ゼロ。
//   - `rvpm add` 時の自動 `on_cmd` 提案 — ユーザが lazy 化したいとき用に
//     コマンド候補リストを出す (future).
//
// 対応フォーマット:
//   - Lua:  `vim.api.nvim_create_user_command("Foo", ...)` / 単引用符も OK
//   - Vim:  `command! [-opts]* Foo ...` / `command -bar Foo ...`
//
// Vim のユーザコマンドは先頭大文字縛り ([A-Z][A-Za-z0-9_]*) — この regex に
// 合わない名前は除外する。理由:
//   - Vim が小文字始まりの command 名を拒否する (E183)
//   - 関数引数 (`require('foo').bar("hoge")`) を誤検出しない自然なフィルタ
//
// 動的定義 (`local name = vim.fn.input(); vim.api.nvim_create_user_command(name,...)`)
// は構造上拾えない — 制約として doc に明記する。#85 issue 参照。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Lua / Vim-script のソース文字列からコマンド名を抽出する。
/// 出現順は保持する。**重複除去は行わない** — 呼び出し側 (`scan_files` 等) で
/// どうせ集約時に dedup するので、ここでは HashSet 作成コストを省いて結果を
/// そのまま返す (PR #86 review で Gemini が指摘)。
pub fn scan_source(src: &str) -> Vec<String> {
    let mut out = Vec::new();

    for line in src.lines() {
        // Lua: vim.api.nvim_create_user_command("Foo", ...)
        //
        // `-- comment` 以降は Lua コメントなので切り捨てる。切らないと
        //   -- example: vim.api.nvim_create_user_command("Example", ...)
        // のような行から偽のコマンド名を拾い、存在しない "Example" が stub
        // として emit されて他 plugin の実 "Example" を shadow する可能性
        // (PR #86 review で CodeRabbit が指摘)。
        // 文字列リテラル内の `--` を誤って切る恐れはあるが、scan 対象は
        // `nvim_create_user_command("Foo", ...)` という明確な形なので、
        // `--` を含むユーザ入力が `"..."` の前に現れるケースは事実上ない。
        //
        // Vim script 側は `" comment` なので strip_prefix("command!") /
        // strip_prefix("command ") の両方に落ちず自然に除外される。
        let lua_code = line.find("--").map_or(line, |i| &line[..i]);
        if let Some(off) = lua_code.find("nvim_create_user_command") {
            let rest = &lua_code[off..];
            if let Some(open) = rest.find('(') {
                let after = rest[open + 1..].trim_start();
                if let Some(name) = extract_quoted_ident(after) {
                    out.push(name);
                }
            }
        }
        // Vim script: `command! [-opts]* Foo ...` / `command [-opts]* Foo ...`
        let trimmed = line.trim_start();
        if let Some(after_cmd) = trimmed
            .strip_prefix("command!")
            .or_else(|| trimmed.strip_prefix("command "))
        {
            let mut rest = after_cmd.trim_start();
            // `-bang`, `-nargs=*`, `-range`, `-bar`, `-complete=…` 等のオプションを飛ばす。
            // `-foo=<arg>` でも間に空白が入らないので単純に `-` で始まる token を
            // 次の空白まで skip すれば OK。
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
    out
}

/// ファイルパスのリストを読み込んで合算したコマンド名リストを返す。
/// 読み込みに失敗したファイルは silently skip (resilience: 1 ファイルが壊れてても
/// 残りのスキャンを続ける)。
pub fn scan_files<P: AsRef<Path>>(paths: &[P]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for p in paths {
        let Ok(src) = std::fs::read_to_string(p) else {
            continue;
        };
        for name in scan_source(&src) {
            if seen.insert(name.clone()) {
                out.push(name);
            }
        }
    }
    out
}

/// プラグイン root から、コマンドを定義しそうなファイル (`plugin/**/*`,
/// `ftplugin/**/*`, `after/plugin/**/*`) を glob して `scan_files` する。
/// `rvpm add` 等で単独プラグインのコマンドを洗い出すための高レベル entrypoint。
pub fn scan_plugin_commands(plugin_root: &Path) -> Vec<String> {
    let mut files: Vec<PathBuf> = Vec::new();
    for sub in ["plugin", "ftplugin", "after/plugin"] {
        let dir = plugin_root.join(sub);
        if !dir.is_dir() {
            continue;
        }
        collect_scan_targets(&dir, &mut files);
    }
    scan_files(&files)
}

/// ディレクトリ配下を再帰で walk して `*.vim` / `*.lua` を集める。
/// 非 UTF-8 パスや read 不能ディレクトリは silently skip (resilience)。
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

/// `"Foo"` または `'Foo'` から `Foo` を取り出す (先頭大文字必須)。
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

/// bare な `Foo` ident を取り出す (先頭大文字必須)。
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

    #[test]
    fn scan_source_picks_lua_nvim_create_user_command() {
        let src = r#"
-- plugin/foo.lua
vim.api.nvim_create_user_command("FooOne", function() end, { bang = true })
vim.api.nvim_create_user_command('FooTwo', function() end, {})
-- not a command, just string arg
require('foo').bar("NotCmd")
"#;
        let mut out = scan_source(src);
        out.sort();
        assert_eq!(out, vec!["FooOne", "FooTwo"]);
    }

    #[test]
    fn scan_source_picks_vim_command_bang_and_options() {
        let src = r#"
" plugin/foo.vim
command! FooOne echo 'one'
command! -bang -nargs=* FooTwo echo 'two'
command -bar FooThree echo 'three'
command! -complete=file -nargs=1 FooFour echo 'four'
"#;
        let mut out = scan_source(src);
        out.sort();
        assert_eq!(out, vec!["FooFour", "FooOne", "FooThree", "FooTwo"]);
    }

    #[test]
    fn scan_source_requires_uppercase_first_letter() {
        // Vim の E183: user command must start with uppercase. 小文字名は
        // そもそも登録できないので拾わない (誤検出防止も兼ねる)。
        let src = r#"
vim.api.nvim_create_user_command("foo", function() end)
command! bar echo 'x'
vim.api.nvim_create_user_command("Foo", function() end)
"#;
        assert_eq!(scan_source(src), vec!["Foo"]);
    }

    #[test]
    fn scan_source_ignores_lua_comment_out_definitions() {
        // コメントアウトされた `nvim_create_user_command` 呼び出しは拾ってはいけない。
        // 拾うと存在しない command が stub 登録され、他 plugin の同名コマンドを
        // shadow する可能性がある (PR #86 review 起点の regression test)。
        let src = r#"
-- example: vim.api.nvim_create_user_command("Example", function() end)
vim.api.nvim_create_user_command("Real", function() end, {})
"#;
        assert_eq!(scan_source(src), vec!["Real"]);
    }

    #[test]
    fn scan_source_preserves_duplicates() {
        // `scan_source` は dedup しない — 重複除去は `scan_files` / caller 側の責務
        // (PR #86 review で Gemini が指摘、scan_files で必ず dedup されるため scan_source
        // の HashSet コストが二重になっていた)。
        let src = r#"
vim.api.nvim_create_user_command("Foo", function() end)
command! Foo echo 'same name'
vim.api.nvim_create_user_command("Foo", function() end)
"#;
        assert_eq!(scan_source(src), vec!["Foo", "Foo", "Foo"]);
    }

    #[test]
    fn scan_files_dedups_across_sources() {
        // dedup は集約層 (scan_files) で行う。同一ファイル内の重複 + ファイル間の
        // 重複の両方をまとめて除去する。
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.lua");
        let b = tmp.path().join("b.vim");
        std::fs::write(
            &a,
            "vim.api.nvim_create_user_command('Foo', function() end)\n\
             vim.api.nvim_create_user_command('Foo', function() end)",
        )
        .unwrap();
        std::fs::write(&b, "command! Foo echo 'b'").unwrap();
        assert_eq!(scan_files(&[a, b]), vec!["Foo"]);
    }

    #[test]
    fn scan_files_aggregates_across_files() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.lua");
        let b = tmp.path().join("b.vim");
        std::fs::write(
            &a,
            "vim.api.nvim_create_user_command('AlphaCmd', function() end, {})",
        )
        .unwrap();
        std::fs::write(&b, "command! BetaCmd echo 'b'").unwrap();
        let mut out = scan_files(&[a, b]);
        out.sort();
        assert_eq!(out, vec!["AlphaCmd", "BetaCmd"]);
    }

    #[test]
    fn scan_files_skips_unreadable_without_failing() {
        // 存在しないパスは silently skip (resilience)。残りのスキャンを止めない。
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real.lua");
        std::fs::write(
            &real,
            "vim.api.nvim_create_user_command('RealCmd', function() end, {})",
        )
        .unwrap();
        let ghost = tmp.path().join("does_not_exist.lua");
        assert_eq!(scan_files(&[ghost, real]), vec!["RealCmd"]);
    }

    #[test]
    fn scan_plugin_commands_walks_plugin_and_ftplugin_and_after() {
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
            // 対象外ディレクトリ — スキップされるはず
            (
                "lua/ignored",
                "c.lua",
                "vim.api.nvim_create_user_command('Ignored', function() end, {})",
            ),
        ] {
            let dir = root.join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(fname), body).unwrap();
        }
        let mut out = scan_plugin_commands(root);
        out.sort();
        assert_eq!(out, vec!["AfterB", "FtRust", "PluginA"]);
    }
}
