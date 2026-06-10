use super::*;

pub(crate) async fn run_list(no_tui: bool) -> Result<bool> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let mut config = parse_config(&toml_content)?;
    // `cache_root` は起動時の spawn_status_check / no_tui 用。以降は
    // reload_state 内で再 resolve されるので外側では不変で良い。
    // `config_root` は描画 (per-plugin hook アイコン) で毎フレーム使うので
    // `let mut` で保持し、reload! マクロで `c` 編集後の新しい値に差し替える。
    let cache_root = resolve_cache_root(config.options.cache_root.as_deref());
    let mut config_root = resolve_config_root(config.options.config_root.as_deref());
    let mut icons = crate::tui::Icons::from_style(config.options.icons);

    if no_tui {
        // 非対話モード: plain text 出力 (旧 status コマンド相当)
        println!("Checking plugin status...");
        let statuses = fetch_plugin_statuses(&config, &cache_root).await;
        let mut rows: Vec<(String, PluginStatus)> = statuses.into_iter().collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (url, status) in rows {
            match status {
                PluginStatus::Finished => println!("  [Clean]     {}", url),
                PluginStatus::Failed(msg) if msg == "Missing" => println!("  [Missing]   {}", url),
                PluginStatus::Syncing(msg) if msg.contains("Modified") => {
                    println!("  [Modified]  {}", url)
                }
                PluginStatus::Syncing(msg) => println!("  [Outdated]  {} ({})", url, msg),
                PluginStatus::Failed(msg) => println!("  [Error]     {} ({})", url, msg),
                PluginStatus::Waiting => println!("  [Waiting]   {}", url),
            }
        }
        return Ok(false);
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // 先頭に空文字 sentinel を入れて [ Global hooks ] の仮想行にする (rvpm edit
    // と同じ感覚で list TUI からも `e` キーで global edit に飛べるように)。
    // empty URL は spawn_status_check の対象 (= config.plugins) には含まれないため
    // 「Waiting」のままになる心配は無く、ここで Finished に印を付けるだけで OK。
    let mut urls: Vec<String> = vec![String::new()];
    urls.extend(config.plugins.iter().map(|p| p.url.clone()));
    let mut tui_state = TuiState::new(urls);
    tui_state
        .status_map
        .insert(String::new(), PluginStatus::Finished);

    // バックグラウンドでステータスチェック開始 (TUI は即表示)
    let (mut rx, mut set) = spawn_status_check(&config, &cache_root);
    let mut bg_done = false;

    // `b` キーで browse TUI に遷移したいフラグ。ループ終了後 main.rs 側に返す。
    let mut goto_browse = false;

    // サブコマンド実行前の TUI 退避 (raw mode OFF + 通常スクリーン復帰 + カーソル表示)。
    fn leave_tui(
        terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;
        Ok(())
    }

    // サブコマンド完了後に TUI を復帰して状態一式を差し替えるためのローカル
    // マクロ。複数の外側変数を同時にムーブ代入するためクロージャにできず、
    // マクロで反復を畳んでいる。
    macro_rules! reload {
        () => {{
            let (c, s, new_rx, new_set, new_config_root) =
                reload_state(&config_path, &mut terminal, &icons)?;
            icons = crate::tui::Icons::from_style(c.options.icons);
            config = c;
            tui_state = s;
            rx = new_rx;
            set = new_set;
            config_root = new_config_root;
            bg_done = false;
        }};
    }

    /// メッセージを表示して任意のキー入力を待つ。
    fn wait_for_keypress(message: &str) -> Result<()> {
        use std::io::Write;
        print!("{}", message);
        std::io::stdout().flush()?;
        crossterm::terminal::enable_raw_mode()?;
        // run_sync / run_update の TUI 終了直後は crossterm の入力キューに
        // 残留イベント (Resize / KeyRelease / sync 中のスクロール連打) が
        // 残りうるので、read の前に一度 drain する。
        while crossterm::event::poll(std::time::Duration::ZERO).unwrap_or(false) {
            let _ = crossterm::event::read();
        }
        // blocking read ではなくタイムアウト付き poll で読むことで、
        // 想定外の環境でも確実に戻ってくるようにする。
        let res = loop {
            match crossterm::event::poll(std::time::Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(crossterm::event::Event::Key(key))
                        if key.kind == crossterm::event::KeyEventKind::Press =>
                    {
                        break Ok(());
                    }
                    Ok(_) => {}
                    Err(e) => break Err(e.into()),
                },
                Ok(false) => {}
                Err(e) => break Err(e.into()),
            }
        };
        let _ = crossterm::terminal::disable_raw_mode();
        println!();
        res
    }

    // アクション後に config を再読み込みして TUI を復帰し、
    // ステータスチェックはバックグラウンドで走らせる。
    // 失敗しても TUI 状態は戻せるように、alt screen への復帰を最初に行う。
    //
    // fetch_plugin_statuses を同期 await にすると、gix を使った status
    // 取得が Windows で秒単位かかる場合や何らかの理由で詰まった場合に、
    // TUI が完全に無描画のまま固まって見える。起動時と同じ progressive
    // 更新パターンに揃え、main loop 側で受信して描画させる。
    type ReloadState = (
        crate::config::Config,
        TuiState,
        mpsc::Receiver<(String, PluginStatus)>,
        JoinSet<()>,
        PathBuf,
    );
    fn reload_state(
        config_path: &Path,
        terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
        _icons: &crate::tui::Icons,
    ) -> Result<ReloadState> {
        // ── 1. 先に TUI に復帰 ──
        // show_cursor() を事前に呼んでいるので hide_cursor() で戻す。
        // clear() は ratatui の内部バッファを無効化して全セル再描画を強制する
        // (これをしないとサブプロセスが alt screen 外で行った描画のせいで
        //  差分描画が崩れた画面を残したまま戻る)。
        enable_raw_mode()?;
        execute!(terminal.backend_mut(), EnterAlternateScreen)?;
        terminal.clear()?;
        terminal.hide_cursor()?;

        // ── 2. config 再読み込み + status は background で開始 ──
        // `c` ハンドラで options.cache_root / options.config_root が変わった
        // 場合にも追従できるよう、ここで毎回 resolve し直して返す。
        let toml_content = std::fs::read_to_string(config_path)?;
        let config = parse_config(&toml_content)?;
        let cache_root = resolve_cache_root(config.options.cache_root.as_deref());
        let config_root = resolve_config_root(config.options.config_root.as_deref());
        // run_list の入口と同じく [ Global hooks ] sentinel を先頭に積む
        let mut urls: Vec<String> = vec![String::new()];
        urls.extend(config.plugins.iter().map(|p| p.url.clone()));
        let mut tui_state = TuiState::new(urls);
        tui_state
            .status_map
            .insert(String::new(), PluginStatus::Finished);
        let (rx, set) = spawn_status_check(&config, &cache_root);

        // ── 3. 復帰直後に残留イベントを drain ──
        // wait_for_keypress で押したキーの release や連打分が残ると、main
        // loop の最初の poll() で拾われて意図しないアクションが起動して
        // しまうことがあるため、ここで捨てる。
        while crossterm::event::poll(std::time::Duration::ZERO).unwrap_or(false) {
            let _ = crossterm::event::read();
        }

        Ok((config, tui_state, rx, set, config_root))
    }

    // env vars (RVPM_APPNAME / NVIM_APPNAME) は run_list 実行中に変わらないので
    // ループ外で 1 回だけ resolve。draw_list の毎フレーム再呼び出しを避ける。
    let init_lua = nvim_init_lua_path();

    loop {
        // バックグラウンドのステータス更新を非ブロッキングで受信
        if !bg_done {
            while let Ok((url, status)) = rx.try_recv() {
                tui_state.update_status(&url, status);
            }
            if set.is_empty() {
                bg_done = true;
            }
            // JoinSet のタスク完了も drain
            while let Some(Ok(_)) = set.try_join_next() {}
        }

        terminal
            .draw(|f| tui_state.draw_list(f, &config, &config_root, &icons, Some(&init_lua)))?;

        if crossterm::event::poll(std::time::Duration::from_millis(50))?
            && let crossterm::event::Event::Key(key) = crossterm::event::read()?
        {
            if key.kind != crossterm::event::KeyEventKind::Press {
                continue;
            }

            // ── 検索モード: インライン入力 ──
            if tui_state.search_mode {
                match key.code {
                    crossterm::event::KeyCode::Esc => tui_state.search_cancel(),
                    crossterm::event::KeyCode::Enter => tui_state.search_confirm(),
                    crossterm::event::KeyCode::Backspace => tui_state.search_backspace(),
                    crossterm::event::KeyCode::Char(c) => tui_state.search_type(c),
                    _ => {}
                }
                continue;
            }

            match key.code {
                crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc => break,

                // ── Ctrl 修飾キー (plain match より先に判定) ──
                crossterm::event::KeyCode::Char('d')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    tui_state.move_down(10);
                }
                crossterm::event::KeyCode::Char('u')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    tui_state.move_up(10);
                }
                crossterm::event::KeyCode::Char('f')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    tui_state.move_down(20);
                }
                crossterm::event::KeyCode::Char('b')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    tui_state.move_up(20);
                }

                // ── vim-like navigation ──
                crossterm::event::KeyCode::Char('j') | crossterm::event::KeyCode::Down => {
                    tui_state.next()
                }
                crossterm::event::KeyCode::Char('k') | crossterm::event::KeyCode::Up => {
                    tui_state.previous()
                }
                crossterm::event::KeyCode::Char('g') | crossterm::event::KeyCode::Home => {
                    tui_state.go_top();
                }
                crossterm::event::KeyCode::Char('G') | crossterm::event::KeyCode::End => {
                    tui_state.go_bottom();
                }
                crossterm::event::KeyCode::Char('/') => {
                    tui_state.start_search();
                }
                crossterm::event::KeyCode::Char('?') => {
                    tui_state.show_help = !tui_state.show_help;
                }
                crossterm::event::KeyCode::Char('n') => tui_state.search_next(),
                crossterm::event::KeyCode::Char('N') => tui_state.search_prev(),

                // ── actions ──
                crossterm::event::KeyCode::Char('b') => {
                    goto_browse = true;
                    break;
                }
                crossterm::event::KeyCode::Char('c') => {
                    leave_tui(&mut terminal)?;
                    if run_config().await? {
                        run_generate(false).await?;
                    }
                    reload!();
                }
                crossterm::event::KeyCode::Char('e') => {
                    if let Some(url) = tui_state.selected_url() {
                        leave_tui(&mut terminal)?;
                        // [ Global hooks ] sentinel: per-plugin ではなく global edit に飛ばす
                        let edited = if url.is_empty() {
                            run_edit(None, false, false, false, true).await?
                        } else {
                            run_edit(Some(url), false, false, false, false).await?
                        };
                        if edited {
                            run_generate(false).await?;
                        }
                        reload!();
                    }
                }
                crossterm::event::KeyCode::Char('s') => {
                    if let Some(url) = tui_state.selected_url() {
                        if url.is_empty() {
                            // sentinel 行では set 対象が無いので何もしない
                            continue;
                        }
                        leave_tui(&mut terminal)?;
                        if run_set(
                            Some(url),
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
                        .await?
                        {
                            run_generate(false).await?;
                        }
                        reload!();
                    }
                }
                crossterm::event::KeyCode::Char('t') => {
                    if let Some(url) = tui_state.selected_url() {
                        if url.is_empty() {
                            // sentinel 行は per-plugin tune の対象外
                            continue;
                        }
                        leave_tui(&mut terminal)?;
                        // `ai_override=None` で options.ai に従う。`run_tune` は AI=Off /
                        // missing plugin dir などの早期失敗で `Err` を返すが eprintln せず
                        // 抜けてくるので、ここで明示的に表示しないと user は「Press any
                        // key…」だけ見せられて理由が分からない (Gemini PR #101 指摘)。
                        if let Err(e) = run_tune(Some(url), None).await {
                            eprintln!("\nError: {e}");
                        }
                        wait_for_keypress("\nPress any key to return to list...")?;
                        reload!();
                    }
                }
                crossterm::event::KeyCode::Char('S') => {
                    leave_tui(&mut terminal)?;
                    let _ = run_sync(false, false, false, None, false, false).await;
                    wait_for_keypress("\nPress any key to return to list...")?;
                    reload!();
                }
                crossterm::event::KeyCode::Char('R') => {
                    leave_tui(&mut terminal)?;
                    // list TUI の `R` キーは全プラグイン rebuild。Some("") が "all" を意味する。
                    let _ = run_sync(false, false, false, Some(String::new()), false, false).await;
                    wait_for_keypress("\nPress any key to return to list...")?;
                    reload!();
                }
                crossterm::event::KeyCode::Char('u') => {
                    if let Some(url) = tui_state.selected_url() {
                        if url.is_empty() {
                            continue;
                        }
                        leave_tui(&mut terminal)?;
                        let _ = run_update(Some(url)).await;
                        wait_for_keypress("\nPress any key to return to list...")?;
                        reload!();
                    }
                }
                crossterm::event::KeyCode::Char('U') => {
                    leave_tui(&mut terminal)?;
                    let _ = run_update(None).await;
                    wait_for_keypress("\nPress any key to return to list...")?;
                    reload!();
                }
                crossterm::event::KeyCode::Char('d') => {
                    if let Some(url) = tui_state.selected_url() {
                        if url.is_empty() {
                            continue;
                        }
                        leave_tui(&mut terminal)?;
                        let _ = run_remove(Some(url)).await;
                        reload!();
                    }
                }
                _ => {}
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(goto_browse)
}

/// 全プラグインの git 状態を並列で調べ、url -> PluginStatus のマップを返す。
/// 全プラグインのステータスチェックを並列で spawn し、受信用 channel と
/// JoinSet を返す。呼び出し側は progressive に受信して描画するか、一括で
/// await して完了を待つか選べる。
fn spawn_status_check(
    config: &crate::config::Config,
    cache_root: &Path,
) -> (mpsc::Receiver<(String, PluginStatus)>, JoinSet<()>) {
    // Size the channel to the plugin count so a producer never blocks on a full
    // buffer while `fetch_plugin_statuses` is still joining every task before it
    // drains `rx` — that combination deadlocked once a config had >100 plugins
    // (#247). The TUI path drains progressively, so it was never affected.
    let (tx, rx) = mpsc::channel::<(String, PluginStatus)>(config.plugins.len().max(1));
    let mut set = JoinSet::new();
    for plugin in config.plugins.iter() {
        let plugin = plugin.clone();
        let cache_root = cache_root.to_path_buf();
        let tx = tx.clone();
        set.spawn(async move {
            let dst_path = resolve_plugin_dst(&plugin, &cache_root);
            let repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
            let git_status = repo.get_status().await;
            let plugin_status = match git_status {
                crate::git::RepoStatus::Clean => PluginStatus::Finished,
                crate::git::RepoStatus::NotInstalled => PluginStatus::Failed("Missing".to_string()),
                crate::git::RepoStatus::Modified => PluginStatus::Syncing("Modified".to_string()),
                crate::git::RepoStatus::Error(e) => PluginStatus::Failed(e),
            };
            let _ = tx.send((plugin.url.clone(), plugin_status)).await;
        });
    }
    drop(tx);
    (rx, set)
}

async fn fetch_plugin_statuses(
    config: &crate::config::Config,
    cache_root: &Path,
) -> std::collections::HashMap<String, PluginStatus> {
    let (mut rx, mut set) = spawn_status_check(config, cache_root);
    while set.join_next().await.is_some() {}
    let mut result = std::collections::HashMap::new();
    while let Ok((url, status)) = rx.try_recv() {
        result.insert(url, status);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_plugin_statuses_does_not_deadlock_above_channel_capacity() {
        // Regression for #247: with the old fixed channel capacity of 100, a
        // config with >100 plugins deadlocked — every producer blocked on
        // `tx.send().await` once the buffer filled, but `fetch_plugin_statuses`
        // only drains `rx` after `join_next()` has joined them all.
        let mut toml = String::from("[options]\n\n");
        for i in 0..150 {
            toml.push_str(&format!("[[plugins]]\nurl = \"owner/plugin-{i}\"\n\n"));
        }
        let config = crate::config::parse_config(&toml).unwrap();
        let cache_root = tempfile::tempdir().unwrap();
        // No plugin dir exists under cache_root, so every status resolves quickly
        // to "Missing". Guard with a timeout so a future deadlock regression fails
        // loudly here instead of hanging CI indefinitely.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            fetch_plugin_statuses(&config, cache_root.path()),
        )
        .await
        .expect("fetch_plugin_statuses timed out — possible deadlock regression");
        assert_eq!(result.len(), 150);
    }
}
