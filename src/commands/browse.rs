use super::*;

pub(crate) async fn run_browse() -> Result<bool> {
    use crate::browse_tui::BrowseTuiState;

    // cache_root / installed set / readme_command を config から解決。
    // resilience 原則: config.toml が壊れていても browse TUI は defaults で開く。
    let config_path = rvpm_config_path();
    let defaults = || {
        (
            resolve_cache_root(None),
            std::collections::HashSet::<String>::new(),
            None::<Vec<String>>,
        )
    };
    // `c` ハンドラで options.cache_root が変わった場合も追従できるよう `let mut`。
    let (mut cache_root, installed, readme_command) = 'resolve: {
        if !config_path.exists() {
            break 'resolve defaults();
        }
        let toml_content = match std::fs::read_to_string(&config_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("\u{26a0} failed to read {}: {}", config_path.display(), e);
                break 'resolve defaults();
            }
        };
        match parse_config(&toml_content) {
            Ok(config) => {
                let cache = resolve_cache_root(config.options.cache_root.as_deref());
                let set: std::collections::HashSet<String> = config
                    .plugins
                    .iter()
                    .filter_map(|p| installed_full_name(&p.url))
                    .collect();
                let cmd = config
                    .options
                    .browse
                    .readme_command
                    .filter(|v| !v.is_empty());
                (cache, set, cmd)
            }
            Err(e) => {
                eprintln!(
                    "\u{26a0} failed to parse {}: {}. Opening browse with defaults.",
                    config_path.display(),
                    e
                );
                defaults()
            }
        }
    };

    let mut state = BrowseTuiState::new();
    state.installed = installed;
    state.readme_command = readme_command;

    // 初期表示: 人気プラグインをバックグラウンドで取得
    let cache_root_bg = cache_root.clone();
    let popular = tokio::task::spawn_blocking(move || crate::browse::fetch_popular(&cache_root_bg));

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // 人気プラグインの結果を待つ
    if let Ok(Ok(repos)) = popular.await {
        state.set_plugins(repos);
    }

    // README を非同期で取得するためのチャネル
    let (readme_tx, mut readme_rx) = tokio::sync::mpsc::channel::<(String, String)>(1);
    // 外部 renderer (`options.browse.readme_command`) の結果用チャネル。
    // 成功時は `Rendered(key, text)`、失敗時は `Warning(message)` をユーザーに
    // 見せる (title bar の message)。capacity 2 は resize 連打時の drop 防止。
    enum RenderMsg {
        Rendered((String, usize, u16), ratatui::text::Text<'static>),
        Warning(String),
    }
    let (render_tx, mut render_rx) = tokio::sync::mpsc::channel::<RenderMsg>(2);
    let mut last_selected: Option<String> = None;
    // README pane の scroll が変化したら terminal.clear() して diff を無効化する。
    // highlight-code の styled span が zellij 等で残骸を残す問題への belt-and-suspenders。
    let mut last_readme_scroll: u16 = state.readme_scroll;
    // 外部 renderer task の重複 spawn 防止用。`(full_name, content_len, width)`
    // がこれと同じなら再スポーンしない。selection / content / resize いずれかの
    // 変化で key が動けば次のループで spawn される。
    let mut last_render_spawned: Option<(String, usize, u16)> = None;

    // `l` キーで list TUI に遷移したいフラグ。ループ終了後 main.rs 側に返す。
    let mut goto_list = false;

    loop {
        if state.readme_scroll != last_readme_scroll {
            terminal.clear()?;
            last_readme_scroll = state.readme_scroll;
        }
        terminal.draw(|f| state.draw(f))?;

        // README 非同期受信
        if let Ok((full_name, content)) = readme_rx.try_recv()
            && state
                .selected_repo()
                .map(|r| r.full_name == full_name)
                .unwrap_or(false)
        {
            state.readme_content = Some(content);
            state.readme_loading = false;
            // 新 content で external_key_current が変わるので、下の統一 spawn 判定に任せる。
        }

        // 外部 renderer の結果 or 警告を受信。
        if let Ok(msg) = render_rx.try_recv() {
            match msg {
                RenderMsg::Rendered(key, text) => {
                    if state.external_key_matches(&key) {
                        state.readme_external_rendered = Some(text);
                        state.readme_external_key = Some(key);
                    }
                }
                RenderMsg::Warning(text) => {
                    state.message = Some(text);
                }
            }
        }

        // 選択変更時に README を非同期取得。
        // 注意: 外部 render spawn より **先に** やる。そうしないと新 repo が
        // selected、readme_content はまだ旧 repo の内容、という状況で
        // external_key_current() が (新 full_name, 旧 content_len) の混成 key を
        // 吐いて、そのまま spawn されると stale な render が混入する恐れがある。
        let current_selected = state.selected_repo().map(|r| r.full_name.clone());
        if current_selected != last_selected {
            last_selected = current_selected.clone();
            if let Some(repo) = state.selected_repo().cloned() {
                state.readme_loading = true;
                state.readme_content = None;
                state.readme_scroll = 0;
                // 新 repo 用に外部 render state もクリアし、spawn debounce key もリセット。
                // こうしないと新 repo + 旧 content_len が偶然一致したケースで
                // last_render_spawned が spawn を抑制してしまう。
                state.readme_external_rendered = None;
                state.readme_external_key = None;
                last_render_spawned = None;
                // Clear widget だけでは ansi-to-tui の styled span の残骸が
                // 一部ホスト (zellij 等) で残ることがあるため、選択変更時は
                // ratatui の内部バッファを明示的に無効化して全セル再描画を強制する。
                terminal.clear()?;
                let tx = readme_tx.clone();
                let cache_root_bg = cache_root.clone();
                tokio::task::spawn_blocking(move || {
                    let content = crate::browse::fetch_readme(&cache_root_bg, &repo)
                        .unwrap_or_else(|e| format!("Error: {}", e));
                    let _ = tx.blocking_send((repo.full_name.clone(), content));
                });
            }
        }

        // 選択変更 / content 受信 / resize いずれかで key が動いたら、
        // 未 spawn の key なら外部 renderer task を spawn する。
        // `last_render_spawned` が実際に飛ばしたキーの記憶役。
        if let Some(cmd) = state.readme_command.as_ref()
            && state.readme_content.is_some()
            && let Some(key) = state.external_key_current()
            && last_render_spawned.as_ref() != Some(&key)
            && let Some(source) = state.build_external_source()
        {
            last_render_spawned = Some(key.clone());
            let cmd = cmd.clone();
            let w = state.readme_visible_width;
            let h = state.readme_visible_height;
            let tx = render_tx.clone();
            tokio::task::spawn_blocking(move || {
                match crate::external_render::render(&cmd, &source, w, h) {
                    Ok(Some(text)) => {
                        let _ = tx.blocking_send(RenderMsg::Rendered(key, text));
                    }
                    Ok(None) => {
                        let _ = tx.blocking_send(RenderMsg::Warning(
                            "readme_command produced no output (fell back to built-in)".to_string(),
                        ));
                    }
                    Err(e) => {
                        let _ = tx.blocking_send(RenderMsg::Warning(format!(
                            "readme_command failed: {} (fell back to built-in)",
                            e
                        )));
                    }
                }
            });
        }

        // キー入力処理
        if crossterm::event::poll(std::time::Duration::from_millis(50))?
            && let crossterm::event::Event::Key(key) = crossterm::event::read()?
        {
            if key.kind != crossterm::event::KeyEventKind::Press {
                continue;
            }

            // `/` ローカルインクリメンタル検索モード
            if state.search_mode {
                match key.code {
                    crossterm::event::KeyCode::Esc => state.search_cancel(),
                    crossterm::event::KeyCode::Enter => state.search_confirm(),
                    crossterm::event::KeyCode::Backspace => state.search_backspace(),
                    crossterm::event::KeyCode::Char(c) => state.search_type(c),
                    _ => {}
                }
                continue;
            }

            // `S` GitHub API 検索モード (旧 `/` の挙動)
            if state.api_search_mode {
                match key.code {
                    crossterm::event::KeyCode::Esc => {
                        // API 入力だけキャンセル。既存の local `/` 検索の
                        // pattern / matches は保持して n/N を引き続き使えるようにする。
                        state.api_search_mode = false;
                        state.search_input.clear();
                    }
                    crossterm::event::KeyCode::Enter => {
                        state.api_search_mode = false;
                        let query = state.search_input.clone();
                        state.search_input.clear();
                        state.message = Some(format!("Searching '{}'...", query));
                        terminal.draw(|f| state.draw(f))?;
                        let cache_root_bg = cache_root.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            crate::browse::search_plugins(&cache_root_bg, &query)
                        })
                        .await;
                        match result {
                            Ok(Ok(repos)) => {
                                state.message = Some(format!("{} results", repos.len()));
                                state.set_plugins(repos);
                                last_selected = None; // 新しい結果で README 再取得を強制
                            }
                            Ok(Err(e)) => {
                                state.message = Some(format!("Error: {}", e));
                            }
                            Err(e) => {
                                state.message = Some(format!("Error: {}", e));
                            }
                        }
                    }
                    crossterm::event::KeyCode::Backspace => {
                        state.search_input.pop();
                    }
                    crossterm::event::KeyCode::Char(c) => {
                        state.search_input.push(c);
                    }
                    _ => {}
                }
                continue;
            }

            match key.code {
                crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc => break,
                crossterm::event::KeyCode::Tab => {
                    state.toggle_focus();
                }
                crossterm::event::KeyCode::Char('?') => {
                    state.show_help = !state.show_help;
                }
                crossterm::event::KeyCode::Char('/') => {
                    state.start_search();
                }
                crossterm::event::KeyCode::Char('S') => {
                    state.start_api_search();
                }
                crossterm::event::KeyCode::Char('n') => {
                    state.search_next();
                }
                crossterm::event::KeyCode::Char('N') => {
                    state.search_prev();
                }

                // ── Navigation: focus-aware ──
                crossterm::event::KeyCode::Char('j') | crossterm::event::KeyCode::Down => {
                    match state.focus {
                        browse_tui::Focus::List => state.next(),
                        browse_tui::Focus::Readme => state.scroll_readme_down(1),
                    }
                }
                crossterm::event::KeyCode::Char('k') | crossterm::event::KeyCode::Up => {
                    match state.focus {
                        browse_tui::Focus::List => state.previous(),
                        browse_tui::Focus::Readme => state.scroll_readme_up(1),
                    }
                }
                crossterm::event::KeyCode::Char('g') | crossterm::event::KeyCode::Home => {
                    match state.focus {
                        browse_tui::Focus::List => state.go_top(),
                        browse_tui::Focus::Readme => state.readme_scroll = 0,
                    }
                }
                crossterm::event::KeyCode::Char('G') | crossterm::event::KeyCode::End => {
                    match state.focus {
                        browse_tui::Focus::List => state.go_bottom(),
                        browse_tui::Focus::Readme => state.scroll_readme_to_bottom(),
                    }
                }
                crossterm::event::KeyCode::Char('d')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    match state.focus {
                        browse_tui::Focus::List => state.move_down(10),
                        browse_tui::Focus::Readme => state.scroll_readme_down(10),
                    }
                }
                crossterm::event::KeyCode::Char('u')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    match state.focus {
                        browse_tui::Focus::List => state.move_up(10),
                        browse_tui::Focus::Readme => state.scroll_readme_up(10),
                    }
                }
                crossterm::event::KeyCode::Char('f')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    match state.focus {
                        browse_tui::Focus::List => state.move_down(20),
                        browse_tui::Focus::Readme => state.scroll_readme_down(20),
                    }
                }
                crossterm::event::KeyCode::Char('b')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    match state.focus {
                        browse_tui::Focus::List => state.move_up(20),
                        browse_tui::Focus::Readme => state.scroll_readme_up(20),
                    }
                }

                // ── Actions ──
                crossterm::event::KeyCode::Char('l') => {
                    goto_list = true;
                    break;
                }
                crossterm::event::KeyCode::Char('c') => {
                    let _ = disable_raw_mode();
                    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
                    let _ = terminal.show_cursor();
                    // エラーを握り潰さず、message に出して TUI で確認できるようにする。
                    let config_result = run_config().await;
                    let changed = matches!(&config_result, Ok(true));
                    let mut gen_err: Option<String> = None;
                    if changed {
                        if let Err(e) = run_generate(false).await {
                            gen_err = Some(e.to_string());
                        } else if let Ok(toml) = std::fs::read_to_string(&config_path)
                            && let Ok(new_config) = parse_config(&toml)
                        {
                            // options.cache_root が書き換わった可能性があるので
                            // 再 resolve。installed (`✓`) と readme_command も更新。
                            cache_root =
                                resolve_cache_root(new_config.options.cache_root.as_deref());
                            state.installed = new_config
                                .plugins
                                .iter()
                                .filter_map(|p| installed_full_name(&p.url))
                                .collect();
                            state.readme_command = new_config
                                .options
                                .browse
                                .readme_command
                                .filter(|v| !v.is_empty());
                        }
                    }
                    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                    enable_raw_mode()?;
                    terminal.clear()?;
                    terminal.hide_cursor()?;
                    // エディタ退出時に残った KeyRelease / Resize イベントが
                    // そのまま次の match に流れると誤発火する (例: 編集中の
                    // 大文字 `S` が API search トリガーになる等) ので drain。
                    while crossterm::event::poll(std::time::Duration::ZERO).unwrap_or(false) {
                        let _ = crossterm::event::read();
                    }
                    state.message = Some(match (&config_result, &gen_err) {
                        (Err(e), _) => format!("Config edit failed: {}", e),
                        (Ok(true), Some(e)) => format!("Config saved; regenerate failed: {}", e),
                        (Ok(true), None) => "Config saved; loader regenerated".to_string(),
                        (Ok(false), _) => "Config unchanged".to_string(),
                    });
                }
                crossterm::event::KeyCode::Char('s') => {
                    state.sort_mode = state.sort_mode.next();
                    state.sort_plugins();
                    state.message = Some(format!("Sort: {}", state.sort_mode.label()));
                }
                crossterm::event::KeyCode::Char('R') => {
                    crate::browse::clear_search_cache(&cache_root);
                    state.message = Some("Cache cleared. Searching...".to_string());
                    terminal.draw(|f| state.draw(f))?;
                    let cache_root_bg = cache_root.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        crate::browse::fetch_popular(&cache_root_bg)
                    })
                    .await;
                    match result {
                        Ok(Ok(repos)) => {
                            state.message = Some(format!("{} plugins", repos.len()));
                            state.set_plugins(repos);
                            last_selected = None; // 新しい結果で README 再取得を強制
                        }
                        _ => {
                            state.message = Some("Refresh failed".to_string());
                        }
                    }
                }
                crossterm::event::KeyCode::Char('o') => {
                    // ブラウザで開く
                    if let Some(repo) = state.selected_repo() {
                        let url = repo.html_url.clone();
                        let _ = open::that(&url);
                    }
                }
                crossterm::event::KeyCode::Enter => {
                    // config.toml に追加 (installed なら警告のみ)
                    if let Some(repo) = state.selected_repo().cloned() {
                        if state.is_installed(&repo) {
                            state.message = Some(format!("already installed: {}", repo.full_name));
                            continue;
                        }
                        let url = repo.full_name.clone();
                        let _ = disable_raw_mode();
                        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
                        let _ = terminal.show_cursor();

                        println!("Adding {}...", url);
                        // run_add の最小版: config.toml に追記して sync
                        // browse で選ぶ user ほど「scan 結果で提案してほしい」派が
                        // 多い — policy_override は渡さず、config `options.auto_lazy`
                        // (デフォルト `Ask`) に委ねる。browse は always TTY なので
                        // `Ask` なら対話プロンプトが自然に出る。
                        // browse から add する場合も config の `options.ai` を尊重する
                        // (user が AI mode を default にしてれば browse 経由でも AI 経路)。
                        let result = run_add(
                            url.clone(),
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                        )
                        .await;
                        let added = result.is_ok();
                        match result {
                            Ok(_) => println!("Added {} successfully!", url),
                            Err(e) => eprintln!("Failed to add {}: {}", url, e),
                        }

                        // TUI に戻る
                        print!("\nPress any key to return to browse...");
                        use std::io::Write;
                        std::io::stdout().flush().ok();
                        enable_raw_mode()?;
                        loop {
                            if let crossterm::event::Event::Key(k) = crossterm::event::read()?
                                && k.kind == crossterm::event::KeyEventKind::Press
                            {
                                break;
                            }
                        }
                        disable_raw_mode()?;
                        execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                        enable_raw_mode()?;
                        // ratatui の内部バッファは LeaveAlternateScreen 中に
                        // run_add() 内の sync TUI や println! が行った描画を知らない。
                        // clear() で全セル再描画を強制し、hide_cursor() で
                        // 先に show_cursor() した状態を戻す。
                        terminal.clear()?;
                        terminal.hide_cursor()?;
                        if added {
                            state.mark_installed(&repo);
                            state.message = Some(format!("Added {}", url));
                        } else {
                            state.message = Some(format!("Failed: {}", url));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    Ok(goto_list)
}
