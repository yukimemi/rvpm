// AI add 用 prompt builder。
//
// 設計指針:
//   - **schema は英語固定** — `<rvpm:plugin_entry>` 等の tag 構造を AI に確実に守らせる
//     ためには指示そのものは英語が安全 (混在で取りこぼす AI がいる)。
//   - **explanation / chat は user の言語** — `options.ai_language` を prompt に
//     差し込んで「この言語で説明して」と頼む。デフォルト "en"。
//   - **token cap** — plugin README + doc/ で巨大な repo (>200KB) もあるので
//     ハード上限 50KB を超えたら trim + 注記を入れる。

use anyhow::Result;
use std::path::Path;

/// rvpm の TOML schema brief (生成時に compile-time に取り込む)。
const SCHEMA: &str = include_str!("schema_prompt.md");

/// AI add の最初の turn で投げる prompt を組み立てる。
///
/// 構成 (ブロック順):
///   1. 役割と出力フォーマット仕様 (schema_prompt.md の英語版)
///   2. 言語ヒント — `<rvpm:explanation>` 内 と chat 応答の言語指示
///   3. 対象プラグイン情報 (URL / README / doc/)
///   4. user の現状 config (config.toml 全文 + plugins/ ツリー一覧)
///   5. 最終インストラクション
pub fn build_initial_prompt(
    plugin_url: &str,
    plugin_root: &Path,
    user_config_toml: &str,
    user_plugins_tree: &str,
    ai_language: &str,
) -> Result<String> {
    let plugin_readme = read_plugin_readme(plugin_root);
    let plugin_doc = read_plugin_doc(plugin_root);

    let mut out = String::new();
    out.push_str(SCHEMA);
    out.push_str("\n\n---\n\n");

    // 言語ヒント — schema 構造は英語固定だが explanation は user 言語
    if !ai_language.eq_ignore_ascii_case("en") {
        out.push_str(&format!(
            "## Language\n\n\
             Respond in **{ai_language}** for natural-language portions: the \
             `<rvpm:explanation>` body and any chat replies after this turn. \
             Keep XML tag names, TOML, and Lua code in their original form (no translation).\n\n",
        ));
    }

    out.push_str("---\n\n");
    out.push_str("# Plugin to add\n\n");
    out.push_str(&format!("URL: `{plugin_url}`\n\n"));
    if let Some(readme) = plugin_readme {
        out.push_str("## README\n\n");
        out.push_str(&trim_to_cap(&readme, 30_000));
        out.push_str("\n\n");
    }
    if let Some(doc) = plugin_doc {
        out.push_str("## Vim help (doc/)\n\n");
        out.push_str(&trim_to_cap(&doc, 15_000));
        out.push_str("\n\n");
    }

    out.push_str("---\n\n");
    out.push_str("# User context\n\n");
    out.push_str("## Current config.toml\n\n");
    out.push_str("```toml\n");
    out.push_str(&trim_to_cap(user_config_toml, 30_000));
    out.push_str("\n```\n\n");

    out.push_str("## Existing plugins/ directory tree\n\n");
    out.push_str("```\n");
    out.push_str(user_plugins_tree.trim_end());
    out.push_str("\n```\n\n");

    out.push_str("---\n\n");
    out.push_str(
        "Now propose the optimal `[[plugins]]` block for the plugin above, plus any \
         hook files. Output exactly the XML tag structure shown earlier — no markdown \
         code fences around the tags, no preamble text outside the tags.\n",
    );

    Ok(out)
}

/// AI tune (`rvpm tune`) 用の最初の turn の prompt を組み立てる。
///
/// `build_initial_prompt` との違い:
///   - **既存の `[[plugins]]` entry を入力に含める** — AI に「これを改善して」と
///     渡す。`current_entry_toml` には config.toml から抜き出した当該プラグインの
///     entry をそのまま入れる。
///   - **タスクの主旨が改善** — 新規追加ではなく、既に動いている設定の tune-up。
///     AI には `on_*` の追加 / 削除、不要 field の trim、より良い trigger の提案、
///     before/after.lua の見直しを依頼する。
///
/// 出力フォーマット (XML tag) は `build_initial_prompt` と完全共通。chat loop /
/// proposal parse / Apply 周りのコードを使い回せる。
pub fn build_tune_prompt(
    plugin_url: &str,
    plugin_root: &Path,
    current_entry_toml: &str,
    user_config_toml: &str,
    user_plugins_tree: &str,
    ai_language: &str,
) -> Result<String> {
    let plugin_readme = read_plugin_readme(plugin_root);
    let plugin_doc = read_plugin_doc(plugin_root);

    let mut out = String::new();
    out.push_str(SCHEMA);
    out.push_str("\n\n---\n\n");

    if !ai_language.eq_ignore_ascii_case("en") {
        out.push_str(&format!(
            "## Language\n\n\
             Respond in **{ai_language}** for natural-language portions: the \
             `<rvpm:explanation>` body and any chat replies after this turn. \
             Keep XML tag names, TOML, and Lua code in their original form (no translation).\n\n",
        ));
    }

    out.push_str("---\n\n");
    out.push_str("# Plugin to tune\n\n");
    out.push_str(&format!("URL: `{plugin_url}`\n\n"));
    out.push_str(
        "This plugin is **already configured** in the user's `config.toml`. \
         Your job is to **improve** the existing setup — add missing lazy triggers, \
         drop redundant fields, suggest better `on_*` patterns, refine \
         `init.lua` / `before.lua` / `after.lua` if helpful, etc. Feel free to \
         overwrite any field of the existing entry; the user will push back via \
         chat if a particular field should be left alone.\n\n",
    );

    out.push_str("## Current `[[plugins]]` entry\n\n");
    out.push_str("```toml\n");
    out.push_str(current_entry_toml.trim_end());
    out.push_str("\n```\n\n");

    if let Some(readme) = plugin_readme {
        out.push_str("## README\n\n");
        out.push_str(&trim_to_cap(&readme, 30_000));
        out.push_str("\n\n");
    }
    if let Some(doc) = plugin_doc {
        out.push_str("## Vim help (doc/)\n\n");
        out.push_str(&trim_to_cap(&doc, 15_000));
        out.push_str("\n\n");
    }

    out.push_str("---\n\n");
    out.push_str("# User context\n\n");
    out.push_str("## Current config.toml\n\n");
    out.push_str("```toml\n");
    out.push_str(&trim_to_cap(user_config_toml, 30_000));
    out.push_str("\n```\n\n");

    out.push_str("## Existing plugins/ directory tree\n\n");
    out.push_str("```\n");
    out.push_str(user_plugins_tree.trim_end());
    out.push_str("\n```\n\n");

    out.push_str("---\n\n");
    out.push_str(
        "Now propose an **improved** `[[plugins]]` block for the plugin above, \
         plus any hook files. The TOML you emit will fully replace the existing \
         entry — keep `name` / `url` consistent with the current entry but feel \
         free to revise everything else. Output exactly the XML tag structure \
         shown earlier — no markdown code fences around the tags, no preamble \
         text outside the tags.\n",
    );

    Ok(out)
}

/// 後続 turn (user follow-up) 用の prompt を組み立てる。
/// 直前の AI 応答 + user の追加要求を渡し、提案を更新させる。
pub fn build_followup_prompt(
    initial_prompt: &str,
    prior_response: &str,
    user_followup: &str,
) -> String {
    format!(
        "{initial_prompt}\n\n---\n\n\
         # Previous proposal (your last reply)\n\n\
         {prior_response}\n\n---\n\n\
         # User feedback\n\n\
         {user_followup}\n\n\
         Update the proposal to address this feedback. Return the same XML tag structure.\n"
    )
}

/// プラグイン root から README を読む (大文字小文字違い + 拡張子違いに対応)。
fn read_plugin_readme(plugin_root: &Path) -> Option<String> {
    let candidates = [
        "README.md",
        "README",
        "README.rst",
        "Readme.md",
        "readme.md",
    ];
    for name in candidates {
        let path = plugin_root.join(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            return Some(content);
        }
    }
    None
}

/// プラグイン doc/ 配下の `*.txt` を結合して返す (Vim help)。50KB 超は trim。
fn read_plugin_doc(plugin_root: &Path) -> Option<String> {
    let doc_dir = plugin_root.join("doc");
    if !doc_dir.is_dir() {
        return None;
    }
    let mut combined = String::new();
    let entries = std::fs::read_dir(&doc_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            combined.push_str(&format!("\n\n=== doc/{name} ===\n\n"));
            combined.push_str(&content);
        }
    }
    if combined.is_empty() {
        None
    } else {
        Some(combined)
    }
}

/// `cap` バイト超なら切って "...(truncated)" を付ける。マルチバイト境界を尊重。
fn trim_to_cap(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    // char boundary を尊重して cap 以下に収める
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n...(truncated, {} bytes total, showing first {} bytes)",
        &text[..end],
        text.len(),
        end
    )
}

/// user の `<config_root>/plugins/` ディレクトリツリーを文字列化 (depth 3 まで)。
/// 「どんな per-plugin hook を持ってるか」を AI に把握させて duplicate 提案を防ぐ。
pub fn collect_plugins_tree(plugins_root: &Path) -> String {
    let mut out = String::new();
    let _ = walk_tree(plugins_root, plugins_root, 0, 3, &mut out);
    if out.is_empty() {
        "(no plugins/ directory yet)".to_string()
    } else {
        out
    }
}

fn walk_tree(
    root: &Path,
    cur: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut String,
) -> std::io::Result<()> {
    if depth > max_depth {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(cur)?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            out.push_str(&format!("{rel}/\n"));
            let _ = walk_tree(root, &path, depth + 1, max_depth, out);
        } else {
            out.push_str(&format!("{rel}\n"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_initial_prompt_includes_schema_and_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("plugin");
        std::fs::create_dir_all(&plugin_root).unwrap();
        std::fs::write(
            plugin_root.join("README.md"),
            "# my-plugin\n\nUse :Foo to start.",
        )
        .unwrap();

        let prompt = build_initial_prompt(
            "owner/repo",
            &plugin_root,
            "[[plugins]]\nurl = \"existing/dep\"\n",
            "github.com/existing/dep/\n",
            "en",
        )
        .unwrap();

        assert!(prompt.contains("rvpm — TOML schema brief"));
        assert!(prompt.contains("owner/repo"));
        assert!(prompt.contains("Use :Foo to start"));
        assert!(prompt.contains("existing/dep"));
        // 英語デフォルトでは Language ヒントは挿入しない
        assert!(!prompt.contains("Respond in"));
    }

    #[test]
    fn build_initial_prompt_inserts_language_hint_when_non_english() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("plugin");
        std::fs::create_dir_all(&plugin_root).unwrap();
        std::fs::write(plugin_root.join("README.md"), "x").unwrap();

        let prompt = build_initial_prompt("owner/repo", &plugin_root, "", "(empty)", "ja").unwrap();
        assert!(prompt.contains("Respond in **ja**"));
    }

    #[test]
    fn build_tune_prompt_includes_current_entry_and_tune_framing() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("plugin");
        std::fs::create_dir_all(&plugin_root).unwrap();
        std::fs::write(plugin_root.join("README.md"), "# tune-me\n\nUse :Bar.").unwrap();

        let current_entry =
            "[[plugins]]\nname = \"tune-me\"\nurl = \"owner/tune-me\"\non_cmd = [\"Bar\"]\n";
        let prompt = build_tune_prompt(
            "owner/tune-me",
            &plugin_root,
            current_entry,
            "[[plugins]]\nurl = \"owner/tune-me\"\n",
            "(empty)",
            "en",
        )
        .unwrap();

        assert!(prompt.contains("Plugin to tune"));
        assert!(prompt.contains("already configured"));
        assert!(prompt.contains("Current `[[plugins]]` entry"));
        assert!(prompt.contains("on_cmd = [\"Bar\"]"));
        assert!(prompt.contains("Use :Bar"));
        assert!(prompt.contains("rvpm — TOML schema brief"));
        // Tune モードでも "improved" を強調
        assert!(prompt.contains("improved"));
    }

    #[test]
    fn build_tune_prompt_inserts_language_hint_when_non_english() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("plugin");
        std::fs::create_dir_all(&plugin_root).unwrap();
        std::fs::write(plugin_root.join("README.md"), "x").unwrap();

        let prompt = build_tune_prompt(
            "owner/repo",
            &plugin_root,
            "[[plugins]]\nurl = \"owner/repo\"\n",
            "",
            "(empty)",
            "ja",
        )
        .unwrap();
        assert!(prompt.contains("Respond in **ja**"));
    }

    #[test]
    fn build_followup_prompt_includes_prior_response_and_feedback() {
        let p = build_followup_prompt(
            "INITIAL",
            "<rvpm:plugin_entry>...</rvpm:plugin_entry>",
            "Add depends = telescope.nvim",
        );
        assert!(p.contains("INITIAL"));
        assert!(p.contains("Previous proposal"));
        assert!(p.contains("<rvpm:plugin_entry>...</rvpm:plugin_entry>"));
        assert!(p.contains("Add depends = telescope.nvim"));
    }

    #[test]
    fn trim_to_cap_truncates_oversized_text() {
        let text = "a".repeat(100);
        let trimmed = trim_to_cap(&text, 30);
        assert!(trimmed.starts_with(&"a".repeat(30)));
        assert!(trimmed.contains("(truncated"));
        assert!(trimmed.contains("100 bytes total"));
    }

    #[test]
    fn trim_to_cap_passes_through_when_under_cap() {
        let text = "short";
        assert_eq!(trim_to_cap(text, 100), "short");
    }

    #[test]
    fn read_plugin_readme_handles_uppercase_and_md_extension() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("README.md"), "hello").unwrap();
        assert_eq!(read_plugin_readme(tmp.path()).as_deref(), Some("hello"));
    }

    #[test]
    fn read_plugin_doc_concatenates_txt_files() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("a.txt"), "AAA").unwrap();
        std::fs::write(doc.join("b.txt"), "BBB").unwrap();
        // 非 txt は無視
        std::fs::write(doc.join("ignored.md"), "MMM").unwrap();
        let combined = read_plugin_doc(tmp.path()).unwrap();
        assert!(combined.contains("AAA"));
        assert!(combined.contains("BBB"));
        assert!(!combined.contains("MMM"));
    }

    #[test]
    fn collect_plugins_tree_lists_files_and_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("github.com").join("owner").join("repo");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("init.lua"), "").unwrap();
        let tree = collect_plugins_tree(tmp.path());
        assert!(tree.contains("github.com/"));
        assert!(tree.contains("github.com/owner/repo/init.lua"));
    }
}
