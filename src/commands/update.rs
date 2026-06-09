use super::*;

pub(crate) async fn run_update(query: Option<String>) -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
    let config_data = parse_config(&toml_content)?;
    let icons = crate::tui::Icons::from_style(config_data.options.icons);
    let config = Arc::new(config_data);
    let cache_root = resolve_cache_root(config.options.cache_root.as_deref());

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
        let plugin = plugin.clone();
        let cache_root = cache_root.clone();
        let tx = tx.clone();
        let sem = semaphore.clone();

        set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            update_single_plugin(&plugin, &cache_root, tx).await
        });
    }

    drop(tx);

    let total_tasks = target_plugins.len();
    let mut finished_tasks = 0;
    let mut update_changes: Vec<crate::update_log::ChangeRecord> = Vec::new();

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
                if let Ok(Ok((plugin, change, head_commit))) = res {
                    if let Some(change) = change {
                        update_changes.push(change_record_from(&plugin, change));
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
