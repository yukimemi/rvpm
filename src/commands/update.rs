use super::*;

pub(crate) async fn run_update(query: Option<String>, no_cooldown: bool) -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
    let config_data = parse_config(&toml_content)?;
    let icons = crate::tui::Icons::from_style(config_data.options.icons);
    let config = Arc::new(config_data);
    let cache_root = resolve_cache_root(config.options.cache_root.as_deref());

    // supply-chain cooldown (#supply-chain): 観測履歴を読み込んでおく。
    // `--no-cooldown` は今回の実行だけゲートを外す (観測の記録もしない —
    // バイパスは「今すぐ tip が欲しい」意思表示なので余計な I/O を増やさない)。
    let cooldown_state_path = resolve_cooldown_state_path(&cache_root);
    let mut cooldown_state = crate::cooldown::CooldownState::load(&cooldown_state_path);

    let target_plugins: Vec<_> = config
        .plugins
        .iter()
        .filter(|p| {
            // dev プラグインは update スキップ
            if p.dev {
                return false;
            }
            if let Some(q) = &query {
                p.url.contains(q.as_str())
                    || p.name
                        .as_deref()
                        .map(|n| n.contains(q.as_str()))
                        .unwrap_or(false)
            } else {
                true
            }
        })
        .cloned()
        .collect();

    if target_plugins.is_empty() {
        println!("No plugins matched the query.");
        return Ok(());
    }

    // `update` は「意図的に最新を取りに行く」操作なので lockfile は **checkout 側には
    // 使わない** (pull 後の新 HEAD で lockfile を上書きするだけ)。config.toml の
    // `rev` は explicit pin なので従来通り従う (gix_checkout がそのまま動く)。
    // query で絞って部分 update する場合も、他プラグインの lockfile entry は残す。
    let config_root_for_lock = resolve_config_root(config.options.config_root.as_deref());
    let lockfile_path = resolve_lockfile_path(&config_root_for_lock);
    let mut lockfile = crate::lockfile::LockFile::load(&lockfile_path);

    let concurrency = resolve_concurrency(config.options.concurrency);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));

    let urls: Vec<String> = target_plugins.iter().map(|p| p.url.clone()).collect();
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;
    let mut tui_state = TuiState::new(urls);
    let (tx, mut rx) = mpsc::channel::<(String, PluginStatus)>(100);

    let mut set = JoinSet::new();

    for plugin in target_plugins.iter() {
        // cooldown ctx の組み立て (#supply-chain):
        // - `--no-cooldown` / 明示 `rev` ピン / 実効 cooldown 0 → ゲート無し
        // - それ以外 → state から観測履歴を引いて渡す。URL 不一致 (同名で別
        //   リポジトリへ差し替え) は別 repo の履歴なので空からやり直す。
        let cooldown_ctx: Option<crate::cooldown::PluginCooldownCtx> =
            if no_cooldown || plugin.rev.is_some() {
                None
            } else {
                let d = crate::cooldown::effective_cooldown(
                    config.options.cooldown.as_deref(),
                    plugin.cooldown.as_deref(),
                );
                if d.is_zero() {
                    None
                } else {
                    let observed = cooldown_state
                        .find(&plugin.display_name())
                        .filter(|e| urls_match(&e.url, &plugin.url))
                        .map(|e| e.observed.clone())
                        .unwrap_or_default();
                    Some(crate::cooldown::PluginCooldownCtx {
                        cooldown: d,
                        observed,
                    })
                }
            };
        let plugin = plugin.clone();
        let cache_root = cache_root.clone();
        let tx = tx.clone();
        let sem = semaphore.clone();

        set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            update_single_plugin(&plugin, &cache_root, tx, cooldown_ctx).await
        });
    }

    drop(tx);

    let total_tasks = target_plugins.len();
    let mut finished_tasks = 0;
    let mut update_changes: Vec<crate::update_log::ChangeRecord> = Vec::new();
    // cooldown で tip 適用を見送った plugin (サマリ表示用)。
    // (display_name, 現 HEAD, held 情報, 実効 cooldown)
    let mut held_by_cooldown: Vec<(
        String,
        Option<String>,
        crate::cooldown::HeldByCooldown,
        std::time::Duration,
    )> = Vec::new();
    let mut cooldown_state_dirty = false;

    while finished_tasks < total_tasks {
        terminal.draw(|f| tui_state.draw(f, "updating...", &icons))?;

        // sync/update 中のイベントキューを drain してスクロール操作を受け付ける
        while crossterm::event::poll(std::time::Duration::from_millis(0))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                tui_state.handle_scroll_key(key, terminal.size()?.height);
            }
        }

        tokio::select! {
            Some((url, status)) = rx.recv() => { tui_state.update_status(&url, status); }
            Some(res) = set.join_next() => {
                finished_tasks += 1;
                if let Ok(Ok((plugin, change, head_commit, cooldown_outcome))) = res {
                    if let Some(change) = change {
                        update_changes.push(change_record_from(&plugin, change));
                    }
                    if let Some(out) = cooldown_outcome {
                        cooldown_state.upsert(crate::cooldown::CooldownEntry {
                            name: plugin.display_name(),
                            url: plugin.url.clone(),
                            observed: out.observed,
                        });
                        cooldown_state_dirty = true;
                        if let Some(held) = out.held {
                            let d = crate::cooldown::effective_cooldown(
                                config.options.cooldown.as_deref(),
                                plugin.cooldown.as_deref(),
                            );
                            held_by_cooldown.push((
                                plugin.display_name(),
                                head_commit.clone(),
                                held,
                                d,
                            ));
                        }
                    }
                    if let Some(commit) = head_commit {
                        lockfile.upsert(crate::lockfile::LockEntry {
                            name: plugin.display_name(),
                            url: plugin.url.clone(),
                            commit,
                        });
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }
    terminal.draw(|f| tui_state.draw(f, "updating...", &icons))?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    // cooldown held-back サマリ (#supply-chain): tip 適用を見送った plugin を
    // 明示する。黙って古いままだと「update したのに進まない」罠になるため。
    if !held_by_cooldown.is_empty() {
        let now = std::time::SystemTime::now();
        held_by_cooldown.sort_by(|a, b| a.0.cmp(&b.0));
        eprintln!(
            "\n{} plugin(s) held back by cooldown (commit too new to trust yet):",
            held_by_cooldown.len()
        );
        for (name, head, held, d) in &held_by_cooldown {
            let age = held
                .tip_first_seen
                .as_deref()
                .map(|t| crate::cooldown::humanize_age(t, now))
                .unwrap_or_else(|| "?".to_string());
            let action = match (&held.fallback, head) {
                (Some(f), _) => format!("advanced to {} instead", crate::update_log::short_hash(f)),
                (None, Some(h)) => format!("kept at {}", crate::update_log::short_hash(h)),
                (None, None) => "kept as-is".to_string(),
            };
            eprintln!(
                "  -> {}  tip {} (first seen {} ago, cooldown {}) — {}",
                name,
                crate::update_log::short_hash(&held.tip),
                age,
                crate::cooldown::humanize_duration(*d),
                action,
            );
        }
        eprintln!(
            "  They will be applied by a later `rvpm update` once they outlive the cooldown.\n  \
             Bypass once with `rvpm update --no-cooldown` (e.g. for a security hotfix)."
        );
    }

    // cooldown 観測履歴の永続化。lockfile と同じく部分 update でも他 plugin の
    // entry は保持する (`retain_by_names` は full sync 側で行う)。cooldown を
    // 使っていない実行ではファイルに触らない (dirty フラグ)。
    if cooldown_state_dirty && let Err(e) = cooldown_state.save(&cooldown_state_path) {
        eprintln!(
            "\u{26a0} failed to save {}: {} (cooldown observations may be stale)",
            cooldown_state_path.display(),
            e
        );
    }

    // 並列 spawn 完了順で積まれているので plugin 名で安定 sort (sync と同じ理由)。
    update_changes.sort_by(|a, b| a.name.cmp(&b.name));
    record_changes_or_warn(&cache_root, "update", update_changes);

    // lockfile save: 部分 update (query 指定) の場合は `retain_by_names` を
    // **かけない** — 更新対象外のプラグインの entry を削りたくないため。
    // ここでは単に新 HEAD を反映した lockfile を atomic write するだけ。
    // (config.toml から外されたプラグインの整理は次の full `rvpm sync` で行う)。
    // chezmoi 連携: source 側に書いてから `chezmoi apply` で target に反映。
    let wp = chezmoi::write_path(config.options.chezmoi, &lockfile_path).await;
    if let Err(e) = lockfile.save(&wp) {
        eprintln!(
            "\u{26a0} failed to save {}: {} (lockfile not updated)",
            wp.display(),
            e
        );
    } else {
        chezmoi::apply(&wp, &lockfile_path).await;
    }

    println!("Update complete. Regenerating loader.lua...");
    run_generate(false).await?;
    Ok(())
}
