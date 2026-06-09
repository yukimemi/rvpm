use super::*;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_add(
    repo: String,
    name: Option<String>,
    lazy: Option<bool>,
    on_cmd: Option<String>,
    on_ft: Option<String>,
    on_map: Option<String>,
    on_event: Option<String>,
    rev: Option<String>,
    policy_override: Option<crate::config::AutoLazyPolicy>,
    ai_override: Option<crate::config::AiBackend>,
) -> Result<()> {
    let config_path = rvpm_config_path();
    ensure_config_exists(&config_path)?;
    let toml_content = std::fs::read_to_string(&config_path)?;
    let mut doc = toml_content.parse::<DocumentMut>()?;
    // url_style は DocumentMut から直接読む (parse_config 経由だと Tera 展開と
    // 全フィールドのデシリアライズが走って無駄。ここでは add に必要な
    // option だけ拾えればよい)。値が無効 / 読めないなら default (Short)。
    let url_style = doc
        .get("options")
        .and_then(|o| o.get("url_style"))
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "short" => Some(crate::config::UrlStyle::Short),
            "full" => Some(crate::config::UrlStyle::Full),
            _ => None,
        })
        .unwrap_or_default();
    if doc.get("plugins").is_none() {
        doc["plugins"] = toml_edit::ArrayOfTables::new().into();
    }
    let plugins = doc["plugins"]
        .as_array_of_tables_mut()
        .context("plugins is not an array of tables")?;
    // 重複検出: browse TUI と同じ `installed_full_name` で両辺を正規化して比較。
    // https / short / ssh / 大文字小文字 / `.git` / 末尾 `/` の揺れを吸収。
    // どちらかが GitHub URL と認識できない (gitlab 等) なら生文字列一致に fallback。
    let incoming_normalized = installed_full_name(&repo);
    for p in plugins.iter() {
        let existing_url = p.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let existing_normalized = installed_full_name(existing_url);
        let matches = match (&incoming_normalized, &existing_normalized) {
            (Some(a), Some(b)) => a == b,
            _ => existing_url == repo,
        };
        if matches {
            println!("Plugin already exists: {}", existing_url);
            return Ok(());
        }
    }
    // 書き込み URL は options.url_style に従って整形 (GitHub 以外はそのまま)。
    let stored_url = format_plugin_url(&repo, url_style);

    // user が明示的に CLI flag で指定したキー一覧 — AI mode 適用時はこれを尊重して
    // 上書きしない (#95 review CodeRabbit Major)。`url` は常に保護 (canonical 化される)。
    let preserved_keys: Vec<&'static str> = {
        let mut k = vec!["url"];
        if name.is_some() {
            k.push("name");
        }
        if lazy.is_some() {
            k.push("lazy");
        }
        if rev.is_some() {
            k.push("rev");
        }
        if on_cmd.is_some() {
            k.push("on_cmd");
        }
        if on_ft.is_some() {
            k.push("on_ft");
        }
        if on_map.is_some() {
            k.push("on_map");
        }
        if on_event.is_some() {
            k.push("on_event");
        }
        k
    };

    let mut new_plugin = table();
    new_plugin["url"] = value(&stored_url);
    if let Some(n) = name {
        new_plugin["name"] = value(n);
    }
    if let Some(l) = lazy {
        new_plugin["lazy"] = value(l);
    }
    if let Some(r) = &rev {
        new_plugin["rev"] = value(r.as_str());
    }
    if let Item::Table(t) = new_plugin {
        plugins.push(t);
    }
    // on_* フラグがあれば set_plugin_list_field / set_plugin_map_field で追加。
    // 検索キーは上で書き込んだ `stored_url` に揃える必要がある — `repo` のままだと
    // `options.url_style = "full"` で `owner/repo` → `https://github.com/owner/repo`
    // に書き換えたとき entry 名とキーが一致せず、no-op になる。
    let maybe_parse = |raw: Option<String>| -> Result<Option<Vec<String>>> {
        raw.map(|s| parse_cli_string_list(&s)).transpose()
    };
    if let Some(items) = maybe_parse(on_cmd)? {
        set_plugin_list_field(&mut doc, &stored_url, "on_cmd", items)?;
    }
    if let Some(items) = maybe_parse(on_ft)? {
        set_plugin_list_field(&mut doc, &stored_url, "on_ft", items)?;
    }
    if let Some(raw) = on_map {
        let specs = parse_on_map_cli(&raw)?;
        set_plugin_map_field(&mut doc, &stored_url, specs)?;
    }
    if let Some(items) = maybe_parse(on_event)? {
        set_plugin_list_field(&mut doc, &stored_url, "on_event", items)?;
    }

    let toml_content = doc.to_string();
    let chezmoi_enabled = read_chezmoi_flag(&config_path);
    chezmoi::write_routed(chezmoi_enabled, &config_path, &toml_content).await?;
    println!("Added plugin to config: {}", stored_url);

    // 追加したプラグインだけ clone + merge し、loader.lua を再生成する
    let config_data = parse_config(&toml_content)?;
    let cache_root = resolve_cache_root(config_data.options.cache_root.as_deref());
    let merged_dir = resolve_merged_dir(&cache_root);

    // `stored_url` は format_plugin_url で canonical 化された URL なので、
    // config.toml に書き込んだ entry とそのまま一致する。`repo` (入力のまま) で
    // 引くと url_style="full" のときミスマッチで clone が走らなくなる。
    let mut add_changes: Vec<crate::update_log::ChangeRecord> = Vec::new();
    // 新規追加したプラグインの HEAD を lockfile にも記録する (dotfiles
    // にコミットすれば他マシンでも同じ commit を再現できる)。
    let config_root_for_lock = resolve_config_root(config_data.options.config_root.as_deref());
    let lockfile_path = resolve_lockfile_path(&config_root_for_lock);
    let mut lockfile = crate::lockfile::LockFile::load(&lockfile_path);
    let mut lockfile_dirty = false;

    // **fetch_state も load しておく**: clone 完了後に `last_fetched` を upsert
    // するため。これをやらないと、`rvpm add` 直後に `rvpm sync` を回したとき、
    // `fetch_state.find(name) → None` で should_fetch が "fetch する" を返し、
    // 直前 clone した repo に対して即 fetch を発射する。Docker overlay /
    // WSL2 のような race の起きやすい FS で gix の "Failed to update
    // references to their new position to match their remote locations"
    // を踏みやすい (user 報告)。run_sync が done している upsert と等価な
    // 後始末を run_add も負担すべき。
    let fetch_state_path = resolve_fetch_state_path(&cache_root);
    let mut fetch_state = crate::fetch_state::FetchState::load(&fetch_state_path);
    let mut fetch_state_dirty = false;
    if let Some(mut plugin) = config_data
        .plugins
        .iter()
        .find(|p| p.url == stored_url)
        .cloned()
    {
        disable_merge_if_cond(&mut plugin);
        let dst_path = resolve_plugin_dst(&plugin, &cache_root);

        println!("Syncing {}...", plugin.display_name());
        let git_repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
        match git_repo.sync().await {
            Err(e) => {
                eprintln!("Warning: failed to sync '{}': {}", plugin.display_name(), e);
            }
            Ok(change) => {
                if let Some(c) = change {
                    add_changes.push(change_record_from(&plugin, c));
                }
                if let Some(err) =
                    execute_build_command(&plugin, &dst_path, &config_data, &cache_root).await
                {
                    eprintln!("Warning: {}: {}", plugin.display_name(), err);
                }
                // lockfile 記録 (no-op sync でも head_commit は読める)
                if !plugin.dev
                    && let Ok(commit) = git_repo.head_commit().await
                {
                    lockfile.upsert(crate::lockfile::LockEntry {
                        name: plugin.display_name(),
                        url: plugin.url.clone(),
                        commit,
                    });
                    lockfile_dirty = true;
                }

                // fetch_state はこのブロックの末尾で upsert する (CodeRabbit PR
                // #107 指摘)。AI / auto-lazy の patch path が config.toml の
                // `name` を変更する可能性があるため、`plugin` のスナップショット
                // ではなく **AI 適用後の最終 name** で記録する必要がある。
                // 実際の upsert は Ok-arm の最後で `read_persisted_plugin_name`
                // を使って行う。

                // run_add 直後に merge する必要は無い: 末尾で `run_generate()` を呼び、
                // そこで merged/ を rm -rf して全 eager+merge を再構築するため。
                // (旧実装はここで merge していたが run_generate に上書きされて冗長)
                let _ = (&plugin, &dst_path, &merged_dir);

                // ── ここで mode 分岐 ────────────────────────────
                // AI 設定が `Off` 以外なら静的 scan を skip して AI 経路 (#93)。
                // それ以外は従来の auto-lazy scan + prompt (#87) に流す。
                let effective_ai = ai_override.unwrap_or(config_data.options.ai);
                if let Ok(backend) = crate::ai::Backend::try_from(effective_ai) {
                    let cfg_root = resolve_config_root(config_data.options.config_root.as_deref());
                    let plugin_cfg_dir = resolve_plugin_config_dir(&cfg_root, &plugin);
                    match crate::ai::run_ai_add(
                        backend,
                        &stored_url,
                        &dst_path,
                        &plugin_cfg_dir,
                        &cfg_root,
                        &config_path,
                        &config_data.options.ai_language,
                        config_data.options.chezmoi,
                    )
                    .await
                    {
                        Ok(outcome) => match outcome.outcome {
                            crate::ai::ChatOutcome::Applied { hook_changes } => {
                                // user が `[[plugins]]` セクションで "Keep existing" を選んだら
                                // `plugin_entry_toml` は None — stub entry をそのまま残す。
                                if let Some(entry_toml) = outcome.plugin_entry_toml {
                                    let latest = std::fs::read_to_string(&config_path)?;
                                    let mut doc_patch = latest.parse::<DocumentMut>()?;
                                    if let Err(e) = replace_plugin_entry_with_ai_toml(
                                        &mut doc_patch,
                                        &stored_url,
                                        &entry_toml,
                                        &preserved_keys,
                                        MergeMode::Merge,
                                    ) {
                                        eprintln!(
                                            "\u{26a0} failed to apply AI proposal: {e}. Stub entry remains."
                                        );
                                    } else {
                                        let patched = doc_patch.to_string();
                                        chezmoi::write_routed(
                                            config_data.options.chezmoi,
                                            &config_path,
                                            &patched,
                                        )
                                        .await?;
                                        println!(
                                            "Applied AI-proposed config for {} ({} hook(s) written, {} removed).",
                                            plugin.display_name(),
                                            hook_changes.written.len(),
                                            hook_changes.removed.len()
                                        );
                                    }
                                } else {
                                    // `add --ai` には "keep existing" の概念が無い (stub
                                    // entry しか存在しない) ので、ここに来るのは chat /
                                    // apply の regression を示す。silent に stub を残すと
                                    // 「green path に化けたバグ」になるので明示 fail する
                                    // (CodeRabbit PR #104 post-merge レビュー指摘)。
                                    //
                                    // **state note (Gemini PR #105 指摘)**: stub entry は
                                    // run_add の冒頭で既に config.toml に書き込まれている。
                                    // ここでの bail はそれを rollback しないので、stub は
                                    // disk に残ったまま。message でその事実 + リカバリ手段を
                                    // 明示し、「refusing to keep」みたいな誤読しやすい表現は
                                    // 避ける。
                                    anyhow::bail!(
                                        "AI add returned no [[plugins]] proposal for {0}. \
                                         The stub entry written to {1} is left as-is — \
                                         remove it manually with `rvpm remove {0}` or rerun \
                                         `rvpm add` with `--no-ai` for the static-scan path.",
                                        plugin.display_name(),
                                        config_path.display()
                                    );
                                }
                            }
                            crate::ai::ChatOutcome::Skipped => {
                                eprintln!(
                                    "AI proposal skipped — stub entry kept in config.toml. \
                                     Edit manually or rerun `rvpm add {repo} --no-ai` for static-scan mode."
                                );
                            }
                            crate::ai::ChatOutcome::HandedOff => {
                                eprintln!(
                                    "Handed off to {} CLI. rvpm exits — that session controls config.toml from here.",
                                    backend.label()
                                );
                            }
                        },
                        Err(e) => {
                            eprintln!(
                                "\u{26a0} AI add failed: {e:#}. Stub entry kept; rerun with `--no-ai` for static-scan mode."
                            );
                            eprintln!(
                                "\n  Debug knobs (env vars):\n\
                                 \x20 RVPM_AI_DUMP_PROMPT=/tmp/p.md   write the prompt to a file and skip the AI call\n\
                                 \x20 RVPM_AI_NO_MERGED=1             drop the `_merged` variant requirement (helps if\n\
                                 \x20                                 the backend's loop-detection trips on near-duplicate\n\
                                 \x20                                 fresh+merged output, e.g. gemini's _recoverFromLoop)\n\
                                 \x20 RVPM_AI_TIMEOUT_SECS=600        raise the per-call timeout (default 300)"
                            );
                        }
                    }
                } else {
                    // ── auto-lazy scan + prompt (#87) — 従来路 ──────────
                    let policy = resolve_add_lazy_policy(policy_override, &config_data);
                    let skip_for_explicit_eager = plugin.lazy_raw == Some(false);
                    if !skip_for_explicit_eager
                        && !matches!(policy, crate::config::AutoLazyPolicy::Never)
                        && let Some(suggestion) =
                            build_add_suggestion(&crate::plugin_scan::scan_plugin(&dst_path))
                        && let Some(applied) =
                            decide_add_lazy_apply(suggestion, policy, &plugin.display_name())
                    {
                        let latest = std::fs::read_to_string(&config_path)?;
                        let mut doc_patch = latest.parse::<DocumentMut>()?;
                        patch_plugin_entry_triggers(&mut doc_patch, &stored_url, &applied);
                        let patched = doc_patch.to_string();
                        let wp =
                            chezmoi::write_path(config_data.options.chezmoi, &config_path).await;
                        std::fs::write(&wp, &patched)?;
                        chezmoi::apply(&wp, &config_path).await;
                        println!(
                            "Recorded lazy triggers for {} in config.toml.",
                            plugin.display_name()
                        );
                    }
                }

                // fetch_state 記録 (deferred): clone / pull が成功したので
                // 「今 fetch した」とマーク。これで直後の `rvpm sync` が
                // fetch_interval fast-path を通って同じ repo に再 fetch を
                // かけずに済み、Docker overlay / WSL2 で gix が踏みやすい
                // post-fetch ref-update race を避けられる (user 報告)。
                //
                //   - `if !plugin.dev` ガード: lockfile + run_sync と挙動を統一
                //     (Gemini PR #107 指摘)。dev plugin は fetch_state 管理対象外。
                //   - **AI / auto-lazy patch 後の `name`** を読みに行く: 上の
                //     branch で config.toml の `name` フィールドが書き換わって
                //     いる可能性があるため、`plugin.display_name()` のスナップ
                //     ショットではなく `read_persisted_plugin_name` で再取得
                //     (CodeRabbit PR #107 指摘)。
                if !plugin.dev {
                    let final_name =
                        read_persisted_plugin_name(&config_path, &stored_url, &plugin.url);
                    fetch_state.upsert(crate::fetch_state::FetchEntry {
                        name: final_name,
                        url: plugin.url.clone(),
                        last_fetched: crate::fetch_state::now_rfc3339(),
                    });
                    fetch_state_dirty = true;
                }
            }
        }
    }

    record_changes_or_warn(&cache_root, "add", add_changes);

    if lockfile_dirty {
        // chezmoi 連携: source 側に書いてから `chezmoi apply` で target に反映。
        let wp = chezmoi::write_path(config_data.options.chezmoi, &lockfile_path).await;
        if let Err(e) = lockfile.save(&wp) {
            eprintln!(
                "\u{26a0} failed to save {}: {} (lockfile not updated)",
                wp.display(),
                e
            );
        } else {
            chezmoi::apply(&wp, &lockfile_path).await;
        }
    }

    // fetch_state.json は cache_root 配下の local-only データなので chezmoi
    // 経路は通さない (run_sync の終端と同じ扱い)。failure は warn のみ —
    // resilience: 本処理は終わってるので fetch_state save 失敗で全体を倒さない。
    if fetch_state_dirty && let Err(e) = fetch_state.save(&fetch_state_path) {
        eprintln!(
            "\u{26a0} failed to save {}: {} (fetch_state not updated)",
            fetch_state_path.display(),
            e
        );
    }

    run_generate(false).await?;
    Ok(())
}
