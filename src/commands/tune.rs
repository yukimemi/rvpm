use super::*;

pub(crate) async fn run_tune(
    query: Option<String>,
    ai_override: Option<crate::config::AiBackend>,
) -> Result<()> {
    let config_path = rvpm_config_path();
    ensure_config_exists(&config_path)?;
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    if config.plugins.is_empty() {
        return Err(anyhow::anyhow!(
            "No plugins in config.toml. Use `rvpm add <repo>` first."
        ));
    }

    // AI backend を解決。`--no-ai` (Off) や config が Off なら error
    // (tune は AI 専用 — non-AI 経路を提供する意味がない、`set` で代替できる)。
    let effective_ai = ai_override.unwrap_or(config.options.ai);
    let backend = crate::ai::Backend::try_from(effective_ai).map_err(|_| {
        anyhow::anyhow!(
            "rvpm tune requires an AI backend. Set `options.ai` in config.toml \
             or pass `--ai <claude|gemini|codex>`."
        )
    })?;

    let Some(selected_url) =
        select_plugin_url(&config.plugins, query.as_deref(), "Select plugin to tune")?
    else {
        return Ok(());
    };

    let plugin = config
        .plugins
        .iter()
        .find(|p| p.url == selected_url)
        .cloned()
        .context("plugin disappeared after selection")?;

    let cache_root = resolve_cache_root(config.options.cache_root.as_deref());
    let dst_path = resolve_plugin_dst(&plugin, &cache_root);
    if !dst_path.exists() {
        return Err(anyhow::anyhow!(
            "Plugin directory does not exist: {}. Run `rvpm sync` first so the AI can read the README/doc.",
            dst_path.display()
        ));
    }

    // 現在の `[[plugins]]` entry を TOML テキストとして抜き出す。
    let doc = toml_content.parse::<DocumentMut>()?;
    let current_entry_toml = extract_plugin_entry_toml(&doc, &selected_url).ok_or_else(|| {
        anyhow::anyhow!("could not extract current entry for `{selected_url}` from config.toml")
    })?;

    let config_root = resolve_config_root(config.options.config_root.as_deref());
    let plugin_cfg_dir = resolve_plugin_config_dir(&config_root, &plugin);

    println!(
        "\u{1f527} Tuning {} with {} ...",
        plugin.display_name(),
        backend.label()
    );

    match crate::ai::run_ai_tune(
        backend,
        &selected_url,
        &dst_path,
        &plugin_cfg_dir,
        &config_root,
        &config_path,
        &current_entry_toml,
        &config.options.ai_language,
        config.options.chezmoi,
    )
    .await
    {
        Ok(outcome) => match outcome.outcome {
            crate::ai::ChatOutcome::Applied { hook_changes } => {
                // user が `[[plugins]]` セクションで "Keep existing entry" を選んだら
                // `plugin_entry_toml` は None — config.toml は触らず、hook ファイル更新のみ。
                if let Some(entry_toml) = outcome.plugin_entry_toml {
                    let latest = std::fs::read_to_string(&config_path)?;
                    let mut doc_patch = latest.parse::<DocumentMut>()?;
                    // user は preview で fresh / merged を per-section に選択済み。
                    // `Replace` mode で AI が omit した stale field (e.g. 古い `on_cmd`) を消す。
                    if let Err(e) = replace_plugin_entry_with_ai_toml(
                        &mut doc_patch,
                        &selected_url,
                        &entry_toml,
                        &[],
                        MergeMode::Replace,
                    ) {
                        eprintln!(
                            "\u{26a0} failed to apply AI proposal: {e}. Existing entry kept."
                        );
                    } else {
                        let patched = doc_patch.to_string();
                        chezmoi::write_routed(config.options.chezmoi, &config_path, &patched)
                            .await?;
                        println!(
                            "Tuned {} ({} hook(s) written, {} removed).",
                            plugin.display_name(),
                            hook_changes.written.len(),
                            hook_changes.removed.len()
                        );
                    }
                } else {
                    println!(
                        "Kept existing entry for {} ({} hook(s) written, {} removed).",
                        plugin.display_name(),
                        hook_changes.written.len(),
                        hook_changes.removed.len()
                    );
                }
            }
            crate::ai::ChatOutcome::Skipped => {
                eprintln!("AI proposal skipped \u{2014} existing entry kept in config.toml.");
            }
            crate::ai::ChatOutcome::HandedOff => {
                eprintln!(
                    "Handed off to {} CLI. rvpm exits \u{2014} that session controls config.toml from here.",
                    backend.label()
                );
            }
        },
        Err(e) => {
            eprintln!("\u{26a0} AI tune failed: {e:#}. Existing entry kept unchanged.");
            eprintln!(
                "\n  Debug knobs (env vars):\n\
                 \x20 RVPM_AI_DUMP_PROMPT=/tmp/p.md   write the prompt to a file and skip the AI call\n\
                 \x20 RVPM_AI_NO_MERGED=1             drop the `_merged` variant requirement (force off)\n\
                 \x20 RVPM_AI_FORCE_MERGED=1          force `_merged` on for Gemini (auto-disabled\n\
                 \x20                                 by default because gemini-cli v0.39's loop guard\n\
                 \x20                                 aborts on near-duplicate fresh+merged output)\n\
                 \x20 RVPM_AI_TIMEOUT_SECS=600        raise the per-call timeout (default 300)"
            );
        }
    }

    run_generate(false).await?;
    Ok(())
}
