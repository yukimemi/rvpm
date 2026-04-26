// AI add の interactive chat loop (Mode A) と applied 提案の preview/apply UI。
//
// Mode B (handoff) は `mod.rs` の `run_handoff` に分離。

use crate::ai::prompt::{
    ExistingHooks, build_followup_prompt, build_initial_prompt, collect_plugins_tree,
};
use crate::ai::{
    Backend, HookChoice, HookWriteDecisions, Proposal, ProposalSection, ensure_cli_installed,
    invoke_oneshot, parse_proposal, run_handoff, validate_proposal_toml, write_hook_files,
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
///   - `Ok(AiAddOutcome { outcome: Applied, plugin_entry_toml: Some(_), .. })` —
///     呼び出し側 (`run_add`) が `plugin_entry_toml` を `config.toml` に append。
///     `None` が返ったら user が "Keep existing entry" を選んだ意味で、config は触らない。
///   - `Ok(_, Skipped)` — user 取り消し。
///   - `Ok(_, HandedOff)` — Mode B エスケープ済み。
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
    let existing_hooks = read_existing_hooks(plugin_config_dir);

    let initial_prompt = build_initial_prompt(
        plugin_url,
        plugin_root,
        user_config_toml_path,
        plugin_config_dir,
        &user_config_toml,
        &user_plugins_tree,
        &existing_hooks,
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
        existing_hooks,
        /* existing_plugin_entry */ None,
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
    let existing_hooks = read_existing_hooks(plugin_config_dir);

    let initial_prompt = crate::ai::prompt::build_tune_prompt(
        plugin_url,
        plugin_root,
        user_config_toml_path,
        plugin_config_dir,
        current_entry_toml,
        &user_config_toml,
        &user_plugins_tree,
        &existing_hooks,
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
        existing_hooks,
        /* existing_plugin_entry */ Some(current_entry_toml.to_string()),
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

/// per-plugin config dir から既存 hook ファイル本文を読む。読めなければ `None`
/// (= AI に既存ファイルを見せない、merged variant も要求しない)。
fn read_existing_hooks(plugin_dir: &Path) -> ExistingHooks {
    let read_one = |name: &str| -> Option<String> {
        let path = plugin_dir.join(name);
        std::fs::read_to_string(&path).ok()
    };
    ExistingHooks {
        init_lua: read_one("init.lua"),
        before_lua: read_one("before.lua"),
        after_lua: read_one("after.lua"),
    }
}

/// AI との対話ループ本体。`run_ai_add` / `run_ai_tune` の共通中核。
///
/// `initial_prompt` を最初に投げ、Apply / Chat / HandOff / Skip の選択を loop し、
/// Apply で per-section の user 選択を確定 → hook ファイル書き込み + config 用
/// `plugin_entry_toml` を `AiAddOutcome` に詰めて返す。
///
/// `existing_plugin_entry`: tune mode で current `[[plugins]]` 本文を渡す
/// (Gemini PR #104 指摘 — preview の `plugin_entry` セクションに既存値も並べて
/// 比較できるようにする)。`None` は add mode (まだ stub のみ) の意味も兼ねる:
/// pick dialog では `None` を「fresh しか提示しない (= 自動採用)」シグナルに使う。
#[allow(clippy::too_many_arguments)]
async fn run_chat_loop(
    backend: Backend,
    initial_prompt: String,
    plugin_config_dir: &Path,
    user_config_toml_path: &Path,
    existing_hooks: ExistingHooks,
    existing_plugin_entry: Option<String>,
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
        print_proposal_preview(
            &proposal,
            plugin_config_dir,
            user_config_toml_path,
            &existing_hooks,
            existing_plugin_entry.as_deref(),
        );

        match prompt_chat_action().await? {
            ChatAction::Apply => {
                let tune_mode = existing_plugin_entry.is_some();
                let (decisions, plugin_entry_toml) =
                    resolve_user_decisions(&proposal, &existing_hooks, tune_mode).await?;
                let written =
                    write_hook_files(plugin_config_dir, &decisions, chezmoi_enabled).await?;
                return Ok(AiAddOutcome {
                    outcome: ChatOutcome::Applied {
                        written_hooks: written,
                    },
                    plugin_entry_toml,
                });
            }
            ChatAction::Skip => {
                return Ok(AiAddOutcome {
                    outcome: ChatOutcome::Skipped,
                    plugin_entry_toml: None,
                });
            }
            ChatAction::HandOff => {
                // refine 済み文脈 + **直近の AI 提案** を統合して handoff prompt を作る。
                //
                // user 報告: 旧実装は `last_prompt` (= AI に *次に* 投げる用の prompt)
                // をそのまま渡していたが、これだと AI が「直近どんな提案をしたか」を
                // 知らない状態で対話を始めるため、handoff 先 CLI が最初から違うことを
                // 提案し直す事故が起きる。`proposal_to_xml(&proposal)` で構造化済みの
                // 提案を末尾に追記して、引き継ぎ先 AI が即座に文脈を継承できるようにする。
                let proposal_xml = proposal_to_xml(&proposal);
                let handoff_prompt = format!(
                    "{last_prompt}\n\n\
                     ---\n\n\
                     # rvpm's latest proposal (already shown to the user)\n\n\
                     The user just picked **Hand off** in rvpm after seeing the \
                     proposal below.\n\n\
                     **Do NOT apply this proposal automatically.** The user \
                     handed off precisely so they can discuss it with you \
                     before anything is written. They may want to apply only \
                     specific parts, ask for refinements, or revise the \
                     proposal entirely. Wait for the user's explicit \
                     instruction before running any Edit / Write tools or \
                     touching `config.toml` / hook files.\n\n\
                     When the user does ask you to apply something, use the \
                     absolute paths from the \"On-disk paths\" section above.\n\n\
                     {proposal_xml}\n"
                );
                run_handoff(backend, &handoff_prompt).await?;
                return Ok(AiAddOutcome {
                    outcome: ChatOutcome::HandedOff,
                    plugin_entry_toml: None,
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

/// `run_ai_add` / `run_ai_tune` の戻り値。Applied 時は `plugin_entry_toml` に user が
/// 選んだ TOML 本文 (fresh / merged / `None` = keep existing) が入る。
pub struct AiAddOutcome {
    pub outcome: ChatOutcome,
    /// user が選んだ `[[plugins]]` 本文 (Apply 時のみ Some)。`None` は「keep existing」。
    pub plugin_entry_toml: Option<String>,
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
            "Apply (pick fresh / merged / keep per-file, then write)",
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
    let mut proposal = parse_proposal(response)?;
    // 少なくとも fresh または merged のどちらかが TOML として valid であれば OK。
    // **invalid な variant は提示前に `None` に落とす** — そうしないと user が pick
    // dialog で invalid variant を選んでしまい、`replace_plugin_entry_with_ai_toml`
    // が malformed TOML を config.toml に書き込んで次回 `parse_config` を破壊する
    // (CodeRabbit PR #104 指摘)。
    let fresh_ok = proposal
        .plugin_entry
        .fresh
        .as_deref()
        .map(validate_proposal_toml)
        .map(|r| r.is_ok())
        .unwrap_or(false);
    let merged_ok = proposal
        .plugin_entry
        .merged
        .as_deref()
        .map(validate_proposal_toml)
        .map(|r| r.is_ok())
        .unwrap_or(false);
    if !fresh_ok && !merged_ok {
        // どちらも valid でなければ、より informative な error を返すために再実行。
        if let Some(s) = proposal.plugin_entry.fresh.as_deref() {
            return Err(validate_proposal_toml(s).unwrap_err());
        }
        if let Some(s) = proposal.plugin_entry.merged.as_deref() {
            return Err(validate_proposal_toml(s).unwrap_err());
        }
        return Err(anyhow::anyhow!("AI proposal had no valid plugin entry"));
    }
    if !fresh_ok {
        proposal.plugin_entry.fresh = None;
    }
    if !merged_ok {
        proposal.plugin_entry.merged = None;
    }
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
    let mut out = String::new();
    push_section_xml(&mut out, "plugin_entry", &p.plugin_entry);
    push_section_xml(&mut out, "init_lua", &p.init_lua);
    push_section_xml(&mut out, "before_lua", &p.before_lua);
    push_section_xml(&mut out, "after_lua", &p.after_lua);
    out.push_str("<rvpm:explanation>\n");
    out.push_str(&p.explanation);
    out.push_str("\n</rvpm:explanation>\n");
    out
}

fn push_section_xml(out: &mut String, name: &str, section: &ProposalSection) {
    // fresh は常に emit (中身が無ければ "(none)") — schema は `<rvpm:NAME>` を
    // 必須側で扱っているので、AI に "前 turn の構造" を再提示するときも欠落させない。
    out.push_str(&format!(
        "<rvpm:{name}>\n{}\n</rvpm:{name}>\n",
        section.fresh.as_deref().unwrap_or("(none)")
    ));
    // merged は section.merged が Some のときのみ emit。None で書き出すと AI が
    // "次 turn でも _merged を返さなくていい" と誤解するリスクを避けるため、
    // 「merged なし」を明示するときは tag そのものを省略する。
    if let Some(body) = section.merged.as_deref() {
        out.push_str(&format!(
            "<rvpm:{name}_merged>\n{body}\n</rvpm:{name}_merged>\n"
        ));
    }
}

/// 1 セクションが提示できる選択肢を集計し、user に per-file dialog を出して決定を返す。
///
/// 戻り値:
///   - `HookWriteDecisions`: 3 hook ファイル分の決定 (Keep / Write)
///   - `Option<String>`: `[[plugins]]` 本文 (None = "keep existing")
async fn resolve_user_decisions(
    proposal: &Proposal,
    existing_hooks: &ExistingHooks,
    tune_mode: bool,
) -> Result<(HookWriteDecisions, Option<String>)> {
    // [[plugins]] entry の選択
    // - tune_mode (既存 entry あり) → fresh / merged / keep の三択
    // - add mode (まだ stub のみ) → fresh しか無い → 自動採用 (ask しない)
    let plugin_entry_toml = pick_plugin_entry_decision(&proposal.plugin_entry, tune_mode).await?;

    // 3 hook ファイルそれぞれの選択
    let init_lua = pick_hook_decision(
        "init.lua",
        &proposal.init_lua,
        existing_hooks.init_lua.as_deref(),
    )
    .await?;
    let before_lua = pick_hook_decision(
        "before.lua",
        &proposal.before_lua,
        existing_hooks.before_lua.as_deref(),
    )
    .await?;
    let after_lua = pick_hook_decision(
        "after.lua",
        &proposal.after_lua,
        existing_hooks.after_lua.as_deref(),
    )
    .await?;

    Ok((
        HookWriteDecisions {
            init_lua,
            before_lua,
            after_lua,
        },
        plugin_entry_toml,
    ))
}

/// `[[plugins]]` entry の per-section 選択。
async fn pick_plugin_entry_decision(
    section: &ProposalSection,
    tune_mode: bool,
) -> Result<Option<String>> {
    // add mode: fresh しか提示されない → 自動採用 (skip にしたければ Skip メニューを使う)。
    if !tune_mode {
        return Ok(section.fresh.clone());
    }

    // tune mode: 既存 entry がある前提。fresh / merged / keep を提示。
    let mut choices: Vec<(String, EntryChoiceKind)> = Vec::new();
    if section.fresh.is_some() {
        choices.push((
            "Use FRESH (clean redesign — overwrite existing entry)".to_string(),
            EntryChoiceKind::Fresh,
        ));
    }
    if section.merged.is_some() {
        choices.push((
            "Use MERGED (preserves your fields, adjusts triggers etc.)".to_string(),
            EntryChoiceKind::Merged,
        ));
    }
    choices.push((
        "Keep existing entry (no change)".to_string(),
        EntryChoiceKind::Keep,
    ));

    if choices.len() == 1 {
        // Keep だけが選択肢 (AI が両方 (none) で返した) → 自動的に keep。
        return Ok(None);
    }

    let labels: Vec<String> = choices.iter().map(|(l, _)| l.clone()).collect();
    let pick = pick_index("[[plugins]] entry — choose:", labels).await?;
    Ok(match choices[pick].1 {
        EntryChoiceKind::Fresh => section.fresh.clone(),
        EntryChoiceKind::Merged => section.merged.clone(),
        EntryChoiceKind::Keep => None,
    })
}

#[derive(Clone, Copy)]
enum EntryChoiceKind {
    Fresh,
    Merged,
    Keep,
}

/// 1 hook ファイルの per-section 選択。
async fn pick_hook_decision(
    name: &str,
    section: &ProposalSection,
    existing: Option<&str>,
) -> Result<HookChoice> {
    // 何も提示されない (AI が両方 (none) で返した) なら何もしない。
    if section.is_empty() {
        return Ok(HookChoice::Keep);
    }

    let mut choices: Vec<(String, HookChoiceKind)> = Vec::new();
    if section.fresh.is_some() {
        let label = if existing.is_some() {
            format!("Use FRESH proposal (overwrite existing {name})")
        } else {
            format!("Use FRESH proposal (create {name})")
        };
        choices.push((label, HookChoiceKind::Fresh));
    }
    if section.merged.is_some() {
        choices.push((
            "Use MERGED proposal (preserves your edits, adds AI suggestions)".to_string(),
            HookChoiceKind::Merged,
        ));
    }
    let keep_label = if existing.is_some() {
        format!("Keep existing {name} (no change)")
    } else {
        format!("Skip — don't create {name}")
    };
    choices.push((keep_label, HookChoiceKind::Keep));

    let labels: Vec<String> = choices.iter().map(|(l, _)| l.clone()).collect();
    let pick = pick_index(&format!("{name} — choose:"), labels).await?;
    Ok(match choices[pick].1 {
        HookChoiceKind::Fresh => HookChoice::Write(section.fresh.clone().unwrap()),
        HookChoiceKind::Merged => HookChoice::Write(section.merged.clone().unwrap()),
        HookChoiceKind::Keep => HookChoice::Keep,
    })
}

#[derive(Clone, Copy)]
enum HookChoiceKind {
    Fresh,
    Merged,
    Keep,
}

/// dialoguer の `Select` で 1 choice を選ばせる薄ラッパ。同期 API なので
/// `spawn_blocking` で thread に逃がす。
async fn pick_index(prompt: &str, labels: Vec<String>) -> Result<usize> {
    use dialoguer::{Select, theme::ColorfulTheme};
    let prompt_owned = prompt.to_string();
    tokio::task::spawn_blocking(move || -> Result<usize> {
        Select::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt_owned)
            .items(&labels)
            .default(0)
            .interact()
            .context("failed to read user choice")
    })
    .await
    .context("failed to join blocking dialoguer task")?
}

fn print_proposal_preview(
    p: &Proposal,
    plugin_config_dir: &Path,
    config_toml_path: &Path,
    existing: &ExistingHooks,
    existing_plugin_entry: Option<&str>,
) {
    let rule = "\u{2500}".repeat(60);
    eprintln!();
    eprintln!(
        "\u{1f4dd} [[plugins]] entry to write into {}:",
        config_toml_path.display()
    );
    // Gemini PR #104 指摘: tune mode で既存 entry を AI 提案と並べて見せると比較しやすい。
    // add mode (existing_plugin_entry = None) では従来通り FRESH のみ表示。
    print_section_block(
        "plugin_entry",
        &p.plugin_entry,
        existing_plugin_entry,
        &rule,
    );

    print_hook_section_block(
        "init.lua",
        &p.init_lua,
        plugin_config_dir,
        existing.init_lua.as_deref(),
        &rule,
    );
    print_hook_section_block(
        "before.lua",
        &p.before_lua,
        plugin_config_dir,
        existing.before_lua.as_deref(),
        &rule,
    );
    print_hook_section_block(
        "after.lua",
        &p.after_lua,
        plugin_config_dir,
        existing.after_lua.as_deref(),
        &rule,
    );

    eprintln!();
    eprintln!("\u{1f4ad} AI explanation:");
    for line in p.explanation.lines() {
        eprintln!("  {line}");
    }
    eprintln!();
}

/// `[[plugins]]` entry 用の preview ブロック。fresh / merged の両方があれば並べて出す。
///
/// 各 variant ([EXISTING] / [FRESH] / [MERGED]) を色分け + 短い区切り線つきで
/// 表示する。`console::style` が NO_COLOR / 非 TTY を尊重するので、redirect 時や
/// CI 環境では自動的にプレーン表示にフォールバックする。
fn print_section_block(name: &str, section: &ProposalSection, existing: Option<&str>, rule: &str) {
    use console::style;

    eprintln!("{rule}");

    // 短い区切り線 (各 variant ブロックの本文上に挟む) — section header の `{rule}`
    // とは別の幅 / 文字を使うことで「セクション境界 vs variant 境界」を区別する。
    let sub_rule = "\u{2504}".repeat(40);

    if let Some(body) = existing {
        // EXISTING — yellow + bold (「現状」を warm 系に)
        eprintln!("  {}", style(format!("[EXISTING {name}]")).yellow().bold());
        eprintln!("  {sub_rule}");
        for line in body.lines().take(20) {
            eprintln!("    {line}");
        }
        if body.lines().count() > 20 {
            eprintln!("    ... ({} more lines)", body.lines().count() - 20);
        }
        eprintln!();
    }
    // FRESH — cyan + bold (「ゼロから書き直し」を cool 系に)
    if let Some(body) = section.fresh.as_deref() {
        eprintln!("  {}", style(format!("[FRESH {name}]")).cyan().bold());
        eprintln!("  {sub_rule}");
        for line in body.lines().take(40) {
            eprintln!("    {line}");
        }
        if body.lines().count() > 40 {
            eprintln!("    ... ({} more lines)", body.lines().count() - 40);
        }
    } else {
        eprintln!(
            "  {} {}",
            style(format!("[FRESH {name}]")).cyan().bold(),
            style("(none)").dim()
        );
    }
    // MERGED — magenta + bold (「ハイブリッド」をはっきり別の色に)
    if let Some(body) = section.merged.as_deref() {
        eprintln!();
        eprintln!("  {}", style(format!("[MERGED {name}]")).magenta().bold());
        eprintln!("  {sub_rule}");
        for line in body.lines().take(40) {
            eprintln!("    {line}");
        }
        if body.lines().count() > 40 {
            eprintln!("    ... ({} more lines)", body.lines().count() - 40);
        }
    }
}

/// hook ファイル用の preview ブロック。
fn print_hook_section_block(
    name: &str,
    section: &ProposalSection,
    plugin_dir: &Path,
    existing: Option<&str>,
    rule: &str,
) {
    if section.is_empty() && existing.is_none() {
        return;
    }
    eprintln!();
    eprintln!(
        "\u{1f4c4} {} (target: {}):",
        name,
        plugin_dir.join(name).display()
    );
    print_section_block(name, section, existing, rule);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_to_xml_emits_all_tags_with_present_lua() {
        let p = Proposal {
            plugin_entry: ProposalSection {
                fresh: Some("[[plugins]]\nurl = \"o/r\"".to_string()),
                merged: None,
            },
            init_lua: ProposalSection {
                fresh: Some("vim.g.x = 1".to_string()),
                merged: None,
            },
            before_lua: ProposalSection::default(),
            after_lua: ProposalSection {
                fresh: Some("require('o').setup({})".to_string()),
                merged: None,
            },
            explanation: "two sentence explanation here.".to_string(),
        };
        let xml = proposal_to_xml(&p);
        // すべての fresh tag が含まれる
        assert!(xml.contains("<rvpm:plugin_entry>"));
        assert!(xml.contains("</rvpm:plugin_entry>"));
        assert!(xml.contains("<rvpm:init_lua>"));
        assert!(xml.contains("vim.g.x = 1"));
        // None の Lua は (none) marker で出力する
        assert!(xml.contains("<rvpm:before_lua>\n(none)\n</rvpm:before_lua>"));
        assert!(xml.contains("require('o').setup"));
        assert!(xml.contains("two sentence explanation here."));
        // merged が無いセクションは _merged tag を emit しない
        assert!(!xml.contains("<rvpm:before_lua_merged>"));
        assert!(!xml.contains("<rvpm:after_lua_merged>"));
    }

    #[test]
    fn proposal_to_xml_emits_merged_tag_when_present() {
        let p = Proposal {
            plugin_entry: ProposalSection {
                fresh: Some("[[plugins]]\nurl = \"o/r\"".to_string()),
                merged: Some(
                    r#"[[plugins]]
url = "o/r"
on_cmd = ["Foo"]"#
                        .to_string(),
                ),
            },
            init_lua: ProposalSection::default(),
            before_lua: ProposalSection::default(),
            after_lua: ProposalSection {
                fresh: Some("FRESH".to_string()),
                merged: Some("MERGED".to_string()),
            },
            explanation: "expl".to_string(),
        };
        let xml = proposal_to_xml(&p);
        assert!(xml.contains("<rvpm:plugin_entry_merged>"));
        assert!(xml.contains("on_cmd = [\"Foo\"]"));
        assert!(xml.contains("<rvpm:after_lua_merged>"));
        assert!(xml.contains("MERGED"));
    }

    #[test]
    fn proposal_to_xml_round_trips_through_parse_proposal() {
        // 再構築した XML が元の Proposal にパースし戻せる (next turn の AI が
        // 前 turn の構造化出力を「自分の前の reply」として読める保証)。
        let original = Proposal {
            plugin_entry: ProposalSection {
                fresh: Some("[[plugins]]\nurl = \"a/b\"\non_cmd = [\"X\"]".to_string()),
                merged: Some("[[plugins]]\nurl = \"a/b\"\non_cmd = [\"X\", \"Y\"]".to_string()),
            },
            init_lua: ProposalSection {
                fresh: Some("a = 1".to_string()),
                merged: None,
            },
            before_lua: ProposalSection::default(),
            after_lua: ProposalSection::default(),
            explanation: "expl".to_string(),
        };
        let xml = proposal_to_xml(&original);
        let reparsed = crate::ai::parse_proposal(&xml).unwrap();
        assert_eq!(reparsed.plugin_entry.fresh, original.plugin_entry.fresh);
        assert_eq!(reparsed.plugin_entry.merged, original.plugin_entry.merged);
        assert_eq!(reparsed.init_lua.fresh, original.init_lua.fresh);
        assert_eq!(reparsed.init_lua.merged, None);
        assert!(reparsed.before_lua.fresh.is_none());
        assert!(reparsed.after_lua.fresh.is_none());
        assert_eq!(reparsed.explanation, original.explanation);
    }

    #[test]
    fn proposal_to_xml_is_smaller_than_typical_raw_response() {
        // Raw AI response は preamble + tags + 余分な改行/コードフェンスで膨らむ。
        // proposal_to_xml は構造のみなので大幅に小さくなる (代表的に 5-10x)。
        let p = Proposal {
            plugin_entry: ProposalSection {
                fresh: Some("[[plugins]]\nurl = \"o/r\"".to_string()),
                merged: None,
            },
            init_lua: ProposalSection::default(),
            before_lua: ProposalSection::default(),
            after_lua: ProposalSection::default(),
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

    #[test]
    fn read_existing_hooks_returns_none_for_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("p");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let hooks = read_existing_hooks(&plugin_dir);
        assert!(hooks.is_empty());
    }

    #[test]
    fn read_existing_hooks_returns_some_when_files_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("p");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("after.lua"), "USER CONTENT\n").unwrap();
        let hooks = read_existing_hooks(&plugin_dir);
        assert!(hooks.init_lua.is_none());
        assert!(hooks.before_lua.is_none());
        assert_eq!(hooks.after_lua.as_deref(), Some("USER CONTENT\n"));
    }

    #[test]
    fn parse_and_validate_drops_invalid_fresh_keeps_valid_merged() {
        // CodeRabbit PR #104 指摘: 片方が invalid TOML だった場合、それを `None` に
        // 落として user dialog に提示しないこと。さもないと user が pick して
        // malformed TOML が config.toml に書き込まれる。
        let response = r#"
<rvpm:plugin_entry>
this is not valid TOML at all
</rvpm:plugin_entry>
<rvpm:plugin_entry_merged>
[[plugins]]
url = "owner/repo"
</rvpm:plugin_entry_merged>
<rvpm:explanation>fresh broken, merged ok</rvpm:explanation>
"#;
        let p = parse_and_validate(response).unwrap();
        assert!(
            p.plugin_entry.fresh.is_none(),
            "invalid fresh must be dropped to None"
        );
        assert!(
            p.plugin_entry.merged.is_some(),
            "valid merged must be retained"
        );
    }

    #[test]
    fn parse_and_validate_drops_invalid_merged_keeps_valid_fresh() {
        let response = r#"
<rvpm:plugin_entry>
[[plugins]]
url = "owner/repo"
</rvpm:plugin_entry>
<rvpm:plugin_entry_merged>
also not valid TOML
</rvpm:plugin_entry_merged>
<rvpm:explanation>fresh ok, merged broken</rvpm:explanation>
"#;
        let p = parse_and_validate(response).unwrap();
        assert!(p.plugin_entry.fresh.is_some());
        assert!(p.plugin_entry.merged.is_none());
    }

    #[test]
    fn parse_and_validate_errors_when_both_variants_invalid() {
        let response = r#"
<rvpm:plugin_entry>
not toml
</rvpm:plugin_entry>
<rvpm:plugin_entry_merged>
also not toml
</rvpm:plugin_entry_merged>
<rvpm:explanation>both broken</rvpm:explanation>
"#;
        assert!(parse_and_validate(response).is_err());
    }
}
