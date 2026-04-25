// AI add の interactive chat loop (Mode A) と applied 提案の preview/apply UI。
//
// Mode B (handoff) は `mod.rs` の `run_handoff` に分離。

use crate::ai::prompt::{build_followup_prompt, build_initial_prompt, collect_plugins_tree};
use crate::ai::{
    Backend, Proposal, ensure_cli_installed, invoke_oneshot, parse_proposal, run_handoff,
    validate_proposal_toml, write_hook_files,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// chat loop の終了アクション。
pub enum ChatOutcome {
    /// user が apply を選択。AI 提案を config.toml + hook ファイルに反映済み。
    Applied { written_hooks: Vec<PathBuf> },
    /// user が skip を選択。何も反映しない。
    Skipped,
    /// user が handoff を選択。CLI ツールを直接起動済み (rvpm 側の処理は終了)。
    HandedOff,
}

/// AI mode の add 全体エントリ。
///
/// 引数:
///   - `backend`: 使う CLI (`Claude` / `Gemini` / `Codex`)
///   - `plugin_url`: user が `rvpm add` で渡した URL (config 書き込み時の同定用)
///   - `plugin_root`: clone 済みプラグインの実パス (README/doc 抽出元)
///   - `config_root`: per-plugin hook を書く先のルート
///   - `user_config_toml_path`: user の `config.toml` (read-only に渡す参考情報、書き込みは呼び出し側)
///   - `ai_language`: explanation/chat の言語 (`"en"` / `"ja"` 等)
///
/// 戻り値:
///   - `Ok(ChatOutcome::Applied { plugin_entry_toml, written_hooks })` —
///     呼び出し側 (`run_add`) が `plugin_entry_toml` を `config.toml` に append。
///   - `Ok(ChatOutcome::Skipped)` — user 取り消し。
///   - `Ok(ChatOutcome::HandedOff)` — Mode B エスケープ済み。
///   - `Err(_)` — CLI 不在 / network / parse 不能 等。
// 引数数が clippy too_many_arguments の閾値超だが、各 path / flag は
// caller (`run_add`) 文脈で意味が明確 (struct でまとめると逆に指示性が下がる)
// なので個別 param で受ける。
#[allow(clippy::too_many_arguments)]
pub async fn run_ai_add(
    backend: Backend,
    plugin_url: &str,
    plugin_root: &Path,
    plugin_config_dir: &Path,
    config_root: &Path,
    user_config_toml_path: &Path,
    ai_language: &str,
    chezmoi_enabled: bool,
) -> Result<AiAddOutcome> {
    ensure_cli_installed(backend)?;

    let (user_config_toml, user_plugins_tree) =
        collect_user_context(user_config_toml_path, config_root)?;

    let initial_prompt = build_initial_prompt(
        plugin_url,
        plugin_root,
        &user_config_toml,
        &user_plugins_tree,
        ai_language,
    )?;

    eprintln!(
        "\u{1f916} Asking {} about {} (this may take a moment)...",
        backend.label(),
        plugin_url
    );

    run_chat_loop(
        backend,
        initial_prompt,
        plugin_config_dir,
        user_config_toml_path,
        chezmoi_enabled,
    )
    .await
}

/// AI tune モード (#97 への補完): 既存 plugin 設定の改善提案を求める。
/// `rvpm tune` から呼ぶ。
///
/// `add` との違いは初期 prompt の組み方だけ — chat loop / preview / Apply 周りは
/// 完全共通。AI には「現状の `[[plugins]]` entry をこう書いてる、改善して」と
/// 投げ、出力フォーマット (XML tag) は同じ。
#[allow(clippy::too_many_arguments)]
pub async fn run_ai_tune(
    backend: Backend,
    plugin_url: &str,
    plugin_root: &Path,
    plugin_config_dir: &Path,
    config_root: &Path,
    user_config_toml_path: &Path,
    current_entry_toml: &str,
    ai_language: &str,
    chezmoi_enabled: bool,
) -> Result<AiAddOutcome> {
    ensure_cli_installed(backend)?;

    let (user_config_toml, user_plugins_tree) =
        collect_user_context(user_config_toml_path, config_root)?;

    let initial_prompt = crate::ai::prompt::build_tune_prompt(
        plugin_url,
        plugin_root,
        current_entry_toml,
        &user_config_toml,
        &user_plugins_tree,
        ai_language,
    )?;

    eprintln!(
        "\u{1f916} Asking {} to tune {} (this may take a moment)...",
        backend.label(),
        plugin_url
    );

    run_chat_loop(
        backend,
        initial_prompt,
        plugin_config_dir,
        user_config_toml_path,
        chezmoi_enabled,
    )
    .await
}

/// `run_ai_add` / `run_ai_tune` 共通の前処理: user の `config.toml` を読み出し、
/// `<config_root>/plugins/` のツリー一覧を文字列化する。
///
/// 呼び出し側 (`run_add` / `run_tune`) は AI 起動 **直前** に config.toml を
/// `chezmoi::write_routed` で書き換えているケースがある (例: `run_add` が stub
/// `[[plugins]]` entry を append) ので、in-memory に持っている古い文字列ではなく
/// 必ず disk から読み直す必要がある。Gemini の "pass it directly" 提案
/// (PR #100 review) ではなく重複コード除去だけを採用しているのはこの理由。
fn collect_user_context(
    user_config_toml_path: &Path,
    config_root: &Path,
) -> Result<(String, String)> {
    let toml = std::fs::read_to_string(user_config_toml_path)
        .with_context(|| format!("failed to read {}", user_config_toml_path.display()))?;
    let tree = collect_plugins_tree(config_root);
    Ok((toml, tree))
}

/// AI との対話ループ本体。`run_ai_add` / `run_ai_tune` の共通中核。
///
/// `initial_prompt` を最初に投げ、Apply / Chat / HandOff / Skip の選択を loop し、
/// Apply で `Proposal` を返す。
async fn run_chat_loop(
    backend: Backend,
    initial_prompt: String,
    plugin_config_dir: &Path,
    user_config_toml_path: &Path,
    chezmoi_enabled: bool,
) -> Result<AiAddOutcome> {
    // chat 履歴を反映して handoff に渡せるよう、最後に AI に投げた prompt を覚えておく。
    // 1 turn 目は initial_prompt そのもの、follow-up が走ったらそれで上書き。
    let mut last_prompt = initial_prompt.clone();
    let first_response = invoke_oneshot(backend, &initial_prompt).await?;
    let mut proposal = parse_and_validate(&first_response)?;
    // raw response (preamble 込み) は使わない — follow-up では parsed `Proposal`
    // から compact XML を再構築して投入する (token 圧縮のため)。
    drop(first_response);

    loop {
        print_proposal_preview(&proposal, plugin_config_dir, user_config_toml_path);

        match prompt_chat_action().await? {
            ChatAction::Apply => {
                let written =
                    write_hook_files(plugin_config_dir, &proposal, chezmoi_enabled).await?;
                return Ok(AiAddOutcome {
                    outcome: ChatOutcome::Applied {
                        written_hooks: written,
                    },
                    proposal: Some(proposal),
                });
            }
            ChatAction::Skip => {
                return Ok(AiAddOutcome {
                    outcome: ChatOutcome::Skipped,
                    proposal: None,
                });
            }
            ChatAction::HandOff => {
                // refine 済み文脈を維持: 最後に投げた prompt をそのまま handoff に渡す。
                run_handoff(backend, &last_prompt).await?;
                return Ok(AiAddOutcome {
                    outcome: ChatOutcome::HandedOff,
                    proposal: None,
                });
            }
            ChatAction::Chat => {
                // chat — user の追加要求を 1 行受けて follow-up prompt 構築。
                //
                // **prior 圧縮**: AI が返した raw response (preamble の "I'll propose..."
                // などのプロセ含む) ではなく、parsed `Proposal` から `<rvpm:*>` tag だけを
                // 再構築して投入する (CodeRabbit suggestion #95)。
                //   - raw は 5-15KB / turn の preamble 込みになり得る
                //   - 再構築 XML は ~1KB 程度 (TOML + lua bodies + explanation のみ)
                //   - AI が次 turn で必要な context は構造化された `<rvpm:*>` だけなので
                //     preamble を削っても reasoning chain を損なわない
                //   - long chat で context window を圧迫しなくなる + token コスト減
                let followup = ask_followup().await?;
                if followup.trim().is_empty() {
                    eprintln!("(empty feedback, returning to action menu)");
                    continue;
                }
                let prior_xml = proposal_to_xml(&proposal);
                last_prompt = build_followup_prompt(&initial_prompt, &prior_xml, &followup);
                eprintln!(
                    "\u{1f916} Asking {} for an updated proposal...",
                    backend.label()
                );
                let next_response = invoke_oneshot(backend, &last_prompt).await?;
                proposal = parse_and_validate(&next_response)?;
            }
        }
    }
}

/// `run_ai_add` の戻り値。Applied 時は `proposal` から `plugin_entry_toml` を取り出して
/// config.toml に書き込むのが呼び出し側 (`run_add`) の責務。
pub struct AiAddOutcome {
    pub outcome: ChatOutcome,
    pub proposal: Option<Proposal>,
}

#[derive(Clone, Copy)]
enum ChatAction {
    Apply,
    Chat,
    HandOff,
    Skip,
}

/// dialoguer は同期 API なので `spawn_blocking` で worker thread に逃がす。
/// async fn 内で `.interact()` を直呼びすると Tokio executor を塞ぐ。
async fn prompt_chat_action() -> Result<ChatAction> {
    use dialoguer::{Select, theme::ColorfulTheme};
    let result = tokio::task::spawn_blocking(|| {
        let choices = [
            "Apply (write to config.toml + create hook files)",
            "Chat (refine with feedback)",
            "Hand off to native CLI (rvpm exits, AI continues directly)",
            "Skip (discard proposal, no changes)",
        ];
        Select::with_theme(&ColorfulTheme::default())
            .with_prompt("How should we proceed?")
            .items(choices.as_slice())
            .default(0)
            .interact()
    })
    .await
    .context("failed to join blocking dialoguer task")??;
    Ok(match result {
        0 => ChatAction::Apply,
        1 => ChatAction::Chat,
        2 => ChatAction::HandOff,
        _ => ChatAction::Skip,
    })
}

/// follow-up テキスト入力。`dialoguer::Input::interact_text` を使うと CJK や
/// 絵文字 (Japanese / Chinese / 全角記号 等) の **display width** が code-point
/// count と一致しないため、Backspace で内部 buffer は減ってもターミナル上に
/// 残骸が残る (display は char width で消すべきだが dialoguer は code-point
/// 単位)。
///
/// 解決: `stdin().read_line()` で **terminal の cooked mode に任せる**。OS の
/// terminal driver は CJK Backspace の幅処理を正しくやるので問題が解消する。
/// 副作用として ColorfulTheme の装飾 prompt は失うが、free-text 入力に theme は
/// 不要 (Select の方は単キー入力なので dialoguer のままで OK)。
async fn ask_followup() -> Result<String> {
    use std::io::{self, BufRead, Write};
    tokio::task::spawn_blocking(|| -> Result<String> {
        // ColorfulTheme の `?` prompt prefix を可能な限り再現 (色は付けない)。
        eprint!("? Your feedback for the AI: ");
        io::stderr().flush().ok();
        let mut buf = String::new();
        io::stdin()
            .lock()
            .read_line(&mut buf)
            .context("failed to read user input")?;
        Ok(buf.trim_end_matches(['\r', '\n']).to_string())
    })
    .await
    .context("failed to join blocking input task")?
}

fn parse_and_validate(response: &str) -> Result<Proposal> {
    let proposal = parse_proposal(response)?;
    validate_proposal_toml(&proposal.plugin_entry_toml)?;
    Ok(proposal)
}

/// `Proposal` から `<rvpm:*>` tag だけの compact XML を再構築する。
///
/// follow-up turn の prompt 投入時に **AI の生 response (preamble 込み)** を
/// そのまま re-inject する代わりにこれを使うと、preamble (例: "Sure, I'll
/// propose...", "Based on the README..." 等の prose 混じり) を削れる。
///
/// AI が次 turn で reasoning に必要な情報は `<rvpm:*>` の中身に過不足なく
/// 含まれている (TOML / hook bodies / explanation) ので情報損失ゼロ。
/// 5-15KB / turn 削減され、長 chat の context window 圧迫と token コストを抑える。
fn proposal_to_xml(p: &Proposal) -> String {
    let mut out = String::with_capacity(p.plugin_entry_toml.len() + p.explanation.len() + 256);
    out.push_str("<rvpm:plugin_entry>\n");
    out.push_str(&p.plugin_entry_toml);
    out.push_str("\n</rvpm:plugin_entry>\n");
    for (tag, body) in [
        ("init_lua", p.init_lua.as_deref()),
        ("before_lua", p.before_lua.as_deref()),
        ("after_lua", p.after_lua.as_deref()),
    ] {
        out.push_str(&format!(
            "<rvpm:{tag}>\n{}\n</rvpm:{tag}>\n",
            body.unwrap_or("(none)")
        ));
    }
    out.push_str("<rvpm:explanation>\n");
    out.push_str(&p.explanation);
    out.push_str("\n</rvpm:explanation>\n");
    out
}

fn print_proposal_preview(p: &Proposal, plugin_config_dir: &Path, config_toml_path: &Path) {
    let rule = "\u{2500}".repeat(60);
    eprintln!();
    eprintln!(
        "\u{1f4dd} [[plugins]] entry to merge into {}:",
        config_toml_path.display()
    );
    eprintln!("{rule}");
    for line in p.plugin_entry_toml.lines() {
        eprintln!("  {line}");
    }

    // 各 hook の skip 対象を集めて、最後にサマリブロックを出す。
    // 単発の "already exists" 行を見落として Apply されると AI 提案が無視されたまま
    // になる UX 事故 (Gemini / CodeRabbit 共通指摘) を防ぐため、メニュー直前に
    // 強調表示する。
    let mut skipped: Vec<String> = Vec::new();
    print_hook_section(
        p.init_lua.as_deref(),
        plugin_config_dir,
        "init.lua",
        &rule,
        &mut skipped,
    );
    print_hook_section(
        p.before_lua.as_deref(),
        plugin_config_dir,
        "before.lua",
        &rule,
        &mut skipped,
    );
    print_hook_section(
        p.after_lua.as_deref(),
        plugin_config_dir,
        "after.lua",
        &rule,
        &mut skipped,
    );

    eprintln!();
    eprintln!("\u{1f4ad} AI explanation:");
    for line in p.explanation.lines() {
        eprintln!("  {line}");
    }

    if !skipped.is_empty() {
        eprintln!();
        eprintln!("{rule}");
        eprintln!(
            "\u{26a0}\u{fe0f}  HEADS UP — Apply will SKIP {} existing hook file(s):",
            skipped.len()
        );
        for path in &skipped {
            eprintln!("    [SKIPPED] {path}");
        }
        eprintln!("    Your existing edits are preserved. To apply the AI proposal,");
        eprintln!("    delete the file first or merge manually.");
        eprintln!("{rule}");
    }
    eprintln!();
}

fn print_hook_section(
    body: Option<&str>,
    plugin_dir: &Path,
    name: &str,
    rule: &str,
    skipped: &mut Vec<String>,
) {
    let Some(body) = body else { return };
    let path = plugin_dir.join(name);
    let exists = path.exists();
    let line_count = body.lines().count();

    eprintln!();
    if exists {
        eprintln!(
            "\u{26a0}\u{fe0f}  [SKIPPED] {} already exists ({} line proposal preserved for reference):",
            path.display(),
            line_count,
        );
        skipped.push(path.display().to_string());
    } else {
        eprintln!(
            "\u{1f195} Will create {} ({} lines):",
            path.display(),
            line_count
        );
    }
    eprintln!("{rule}");
    for line in body.lines().take(20) {
        eprintln!("  {line}");
    }
    if line_count > 20 {
        eprintln!("  ... ({} more lines)", line_count - 20);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_to_xml_emits_all_tags_with_present_lua() {
        let p = Proposal {
            plugin_entry_toml: "[[plugins]]\nurl = \"o/r\"".to_string(),
            init_lua: Some("vim.g.x = 1".to_string()),
            before_lua: None,
            after_lua: Some("require('o').setup({})".to_string()),
            explanation: "two sentence explanation here.".to_string(),
        };
        let xml = proposal_to_xml(&p);
        // すべての tag が含まれる
        assert!(xml.contains("<rvpm:plugin_entry>"));
        assert!(xml.contains("</rvpm:plugin_entry>"));
        assert!(xml.contains("<rvpm:init_lua>"));
        assert!(xml.contains("vim.g.x = 1"));
        // None の Lua は (none) marker で出力する
        assert!(xml.contains("<rvpm:before_lua>\n(none)\n</rvpm:before_lua>"));
        assert!(xml.contains("require('o').setup"));
        assert!(xml.contains("two sentence explanation here."));
    }

    #[test]
    fn proposal_to_xml_round_trips_through_parse_proposal() {
        // 再構築した XML が元の Proposal にパースし戻せる (next turn の AI が
        // 前 turn の構造化出力を「自分の前の reply」として読める保証)。
        let original = Proposal {
            plugin_entry_toml: "[[plugins]]\nurl = \"a/b\"\non_cmd = [\"X\"]".to_string(),
            init_lua: Some("a = 1".to_string()),
            before_lua: None,
            after_lua: None,
            explanation: "expl".to_string(),
        };
        let xml = proposal_to_xml(&original);
        let reparsed = crate::ai::parse_proposal(&xml).unwrap();
        assert_eq!(
            reparsed.plugin_entry_toml.trim(),
            original.plugin_entry_toml
        );
        assert_eq!(reparsed.init_lua, original.init_lua);
        assert_eq!(reparsed.before_lua, None); // (none) → None
        assert_eq!(reparsed.after_lua, None);
        assert_eq!(reparsed.explanation, original.explanation);
    }

    #[test]
    fn proposal_to_xml_is_smaller_than_typical_raw_response() {
        // Raw AI response は preamble + tags + 余分な改行/コードフェンスで膨らむ。
        // proposal_to_xml は構造のみなので大幅に小さくなる (代表的に 5-10x)。
        let p = Proposal {
            plugin_entry_toml: "[[plugins]]\nurl = \"o/r\"".to_string(),
            init_lua: None,
            before_lua: None,
            after_lua: None,
            explanation: "Brief".to_string(),
        };
        let compact = proposal_to_xml(&p);
        // Compact 表現は 1KB 未満に収まるべき (簡素な proposal なら)。
        assert!(
            compact.len() < 1024,
            "compact xml unexpectedly large: {} bytes",
            compact.len()
        );
    }
}
