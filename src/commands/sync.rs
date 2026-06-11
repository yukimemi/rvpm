use super::*;

pub(crate) async fn run_sync(
    prune: bool,
    frozen: bool,
    no_lock: bool,
    rebuild: Option<String>,
    refresh: bool,
    no_refresh: bool,
) -> Result<()> {
    // early return (`?` / bail!) でも背景削除スレッドを必ず回収する (Gemini PR #229)。
    let _reap_guard = ReapGuard;
    // `--frozen` は lockfile に依存した strict check、`--no-lock` は lockfile を
    // 完全無視する escape hatch。両方立つと「strict のつもりが silently latest を
    // pull する」矛盾状態になるので、fail fast させる (CI の思い込みを防ぐ)。
    if frozen && no_lock {
        anyhow::bail!(
            "--frozen cannot be combined with --no-lock (they contradict: one requires the lockfile, the other ignores it)"
        );
    }
    // `--refresh` / `--no-refresh` は clap 側の `conflicts_with` で既に弾かれて
    // いるが念のため。
    if refresh && no_refresh {
        anyhow::bail!("--refresh cannot be combined with --no-refresh");
    }
    let refresh_mode = if refresh {
        crate::fetch_state::RefreshMode::Force
    } else if no_refresh {
        crate::fetch_state::RefreshMode::Skip
    } else {
        crate::fetch_state::RefreshMode::Auto
    };

    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

    let mut config_data = parse_config(&toml_content)?;
    crate::config::sort_plugins(&mut config_data.plugins)?;
    for plugin in config_data.plugins.iter_mut() {
        disable_merge_if_cond(plugin);
    }
    let config = Arc::new(config_data);

    let cache_root = resolve_cache_root(config.options.cache_root.as_deref());
    let merged_dir = resolve_merged_dir(&cache_root);
    let views_dir = resolve_views_dir(&cache_root);

    // lockfile: sync 前に load、各 plugin 処理時に効く rev を引き、sync 後の HEAD
    // で上書きして終端で save。`--no-lock` 時は load/save 両方スキップ。
    // `--frozen` は全 non-dev plugin に lockfile entry があることを要求する
    // (無ければ sync 開始前に bail) — CI / fresh machine で strict 再現を狙うケース。
    let config_root_for_lock = resolve_config_root(config.options.config_root.as_deref());
    let lockfile_path = resolve_lockfile_path(&config_root_for_lock);
    let mut lockfile = if no_lock {
        crate::lockfile::LockFile::default()
    } else {
        crate::lockfile::LockFile::load(&lockfile_path)
    };
    if frozen && !no_lock {
        // 2 種類の問題を区別して報告する:
        // - **missing**: lockfile にエントリ自体が無い
        // - **stale**: エントリはあるが `url` が config と食い違っている
        //   (例: 同 display_name で別リポジトリに差し替えた)
        //   → 古い commit を適用すると sync がコケるので未ロック相当として拒否。
        let mut issues: Vec<String> = Vec::new();
        for plugin in config.plugins.iter().filter(|p| !p.dev) {
            match lockfile.find(&plugin.display_name()) {
                None => issues.push(format!("{} (missing)", plugin.display_name())),
                Some(entry) if !urls_match(&entry.url, &plugin.url) => issues.push(format!(
                    "{} (stale: lockfile url={}, config url={})",
                    plugin.display_name(),
                    entry.url,
                    plugin.url,
                )),
                Some(_) => {}
            }
        }
        if !issues.is_empty() {
            anyhow::bail!(
                "--frozen: {} plugin(s) not reproducible from {}:\n  {}",
                issues.len(),
                lockfile_path.display(),
                issues.join("\n  "),
            );
        }
    }
    // plugin name -> 該当 LockEntry の lookup。`commit` に加えて `url` も
    // 保持する理由は、同じ display_name で別リポジトリに差し替えられたケース
    // (例: `owner/foo.nvim` → `different-owner/foo.nvim`) で古い commit を適用
    // しないように URL の一致を確認してから checkout 対象とするため。URL 不一致
    // なら未ロック扱い (sync 完了後の新 HEAD で lockfile 側が正しく上書きされる)。
    let locked_entries: std::collections::HashMap<String, crate::lockfile::LockEntry> = if no_lock {
        std::collections::HashMap::new()
    } else {
        lockfile
            .plugins
            .iter()
            .map(|e| (e.name.clone(), e.clone()))
            .collect()
    };

    // fetch cache: per-plugin 最終 fetch 時刻を読み込んで、staleness window 内の
    // プラグインの fetch をスキップするための判定テーブルを作る。
    // - state の場所は `<cache_root>/fetch_state.json`
    // - `options.fetch_interval` でウィンドウサイズ、CLI の `--refresh` / `--no-refresh`
    //   で上書き可能
    // - state 読み込みは lockfile と同じく malformed 時は empty fallback (resilience)
    let fetch_state_path = resolve_fetch_state_path(&cache_root);
    let mut fetch_state = crate::fetch_state::FetchState::load(&fetch_state_path);
    let fetch_interval =
        crate::fetch_state::resolve_fetch_interval(config.options.fetch_interval.as_deref());
    let now_sys = std::time::SystemTime::now();
    // name → &FetchEntry の lookup map を先に組んで、plugin ごとの find を O(1) に
    // する (fetch_state.find は線形スキャンなので、200+ plugin 構成で pre-compute
    // 全体が O(N^2) になってしまう)。URL 不一致 (= 同じ name で別リポジトリに
    // 差し替えられた等) は lockfile と同じく「未管理」扱いにして last_fetched を
    // 無視する — 旧 URL のタイムスタンプで fetch を省略して、新 URL の fetch を
    // 取りこぼすのを防ぐ。
    let fetch_lookup: std::collections::HashMap<&str, &crate::fetch_state::FetchEntry> =
        fetch_state
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e))
            .collect();
    let fetch_decisions: std::collections::HashMap<String, bool> = config
        .plugins
        .iter()
        .filter(|p| !p.dev)
        .map(|p| {
            let name = p.display_name();
            let last = fetch_lookup
                .get(name.as_str())
                .filter(|e| urls_match(&e.url, &p.url))
                .map(|e| e.last_fetched.as_str());
            (
                name,
                crate::fetch_state::should_fetch(last, now_sys, fetch_interval, refresh_mode),
            )
        })
        .collect();
    let is_hard_skip = refresh_mode == crate::fetch_state::RefreshMode::Skip;

    // supply-chain cooldown (#supply-chain): sync は lockfile pin が新 commit を
    // 既に遮断しているのでゲートはしない。ただし fetch のついでに remote tip の
    // 観測を記録して cooldown の熟成を進める — update を走らせない期間も tip が
    // 熟成していき、次の `rvpm update` がそこまで前進できる。
    let cooldown_state_path = resolve_cooldown_state_path(&cache_root);
    let mut cooldown_state = crate::cooldown::CooldownState::load(&cooldown_state_path);
    let mut cooldown_tracking_any = false;

    if merged_dir.exists() {
        let _ = std::fs::remove_dir_all(&merged_dir);
    }
    std::fs::create_dir_all(&merged_dir)?;

    let icons = crate::tui::Icons::from_style(config.options.icons);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
    let mut tui_state = TuiState::new(urls);
    let (tx, mut rx) = mpsc::channel::<(String, PluginStatus)>(100);

    let concurrency = resolve_concurrency(config.options.concurrency);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut set = JoinSet::new();

    // `--rebuild [QUERY]` のクエリは plugin 数だけループで照合するので、
    // ここで 1 回だけ lowercase して per-plugin の再計算を避ける。
    // `None` / `Some("")` はそのまま、`Some(q)` は `Some(q.to_lowercase())`。
    let rebuild_filter_lc: Option<String> = rebuild.as_deref().map(|q| q.to_ascii_lowercase());

    for plugin in config.plugins.iter() {
        // dev プラグインは sync をスキップ (ローカル開発中のためリセットしない)
        if plugin.dev {
            let dst_path = resolve_plugin_dst(plugin, &cache_root);
            if !dst_path.exists() {
                let _ = tx.try_send((
                    plugin.url.clone(),
                    PluginStatus::Failed(format!(
                        "dev directory not found: {}",
                        dst_path.display()
                    )),
                ));
            } else {
                let _ = tx.try_send((plugin.url.clone(), PluginStatus::Finished));
            }
            continue;
        }
        let plugin = plugin.clone();
        let cache_root = cache_root.clone();
        let tx = tx.clone();
        let sem = semaphore.clone();

        // effective rev: plugin.rev (explicit) > lockfile commit (URL 一致時のみ) > None.
        // URL 不一致時 (同名で別リポジトリに差し替え等) は未ロック扱いにして、誤った
        // commit を適用して sync がコケるのを防ぐ — sync 完了後に新 HEAD で lockfile
        // 側が上書きされて正しい URL/commit の対応に直る。
        // `--no-lock` 時は locked_entries が空なので plugin.rev だけ効く。
        let effective_rev: Option<String> = plugin.rev.clone().or_else(|| {
            locked_entries
                .get(&plugin.display_name())
                .filter(|e| urls_match(&e.url, &plugin.url))
                .map(|e| e.commit.clone())
        });

        // pre-computed fetch decision: true なら通常フロー、false なら fast-path
        // 候補。clone 欠損 or HEAD != effective_rev で fast-path が使えないときは、
        // `--no-refresh` (is_hard_skip) なら error、それ以外なら full flow にフォール。
        let want_fetch = *fetch_decisions.get(&plugin.display_name()).unwrap_or(&true);
        let hard_skip = is_hard_skip;

        // cooldown 観測 ctx (#supply-chain): 実効 cooldown が有効な rev-none
        // plugin だけ観測履歴を持ち込む。URL 不一致 (同名で別リポジトリへ
        // 差し替え) は別 repo の履歴なので空からやり直す (lockfile と同じ思想)。
        let cooldown_ctx: Option<crate::cooldown::PluginCooldownCtx> = if plugin.rev.is_some() {
            None
        } else {
            let d = crate::cooldown::effective_cooldown(
                config.options.cooldown.as_deref(),
                plugin.cooldown.as_deref(),
            );
            if d.is_zero() {
                None
            } else {
                cooldown_tracking_any = true;
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

        // --rebuild [QUERY] のスコープ判定は closure 外で済ませて bool を move する
        // (`rebuild_filter: &Option<String>` のライフタイムを async move に引き込まない)。
        // query は外側で lowercase 済み (`rebuild_filter_lc`)。
        let force_rebuild = matches_rebuild_filter(&plugin, rebuild_filter_lc.as_deref());

        let config_for_build = config.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let dst_path = resolve_plugin_dst(&plugin, &cache_root);
            let _ = tx
                .send((
                    plugin.url.clone(),
                    PluginStatus::Syncing("Syncing...".to_string()),
                ))
                .await;
            let repo = Repo::new(&plugin.url, &dst_path, effective_rev.as_deref());
            // sync 実行判定:
            //   want_fetch=true                         → full flow (git fetch + checkout)
            //   want_fetch=false, HEAD == target        → no-op fast path
            //   want_fetch=false, local checkout 可能    → fast path (fetch なし checkout だけ)
            //   want_fetch=false, local で満たせない     → hard_skip なら error、それ以外は full flow
            //
            // effective_rev が branch/tag でも local DB で SHA に解決してから比較
            // するので `rev = "main"` / `rev = "v1.2.3"` 系でも fast path が効く。
            // None (pin 無し) は「HEAD 自体を target」として no-op 扱い。
            //
            // HEAD != target でも commit が local にあれば、fetch せず checkout
            // だけで満たせる (最近の window 内で別 rev へ切替えたケースなど) ので
            // `checkout_locally` を試して成功すれば fast path 継続、失敗したら
            // full flow に fall through する。
            let mut fetched = false;
            let res: Result<Option<crate::git::GitChange>> = if want_fetch {
                fetched = true;
                repo.sync().await
            } else {
                let head_now = if dst_path.exists() {
                    repo.head_commit().await.ok()
                } else {
                    None
                };
                let target_sha: Option<String> = match effective_rev.as_deref() {
                    None => head_now.clone(),
                    Some(rev) => repo.resolve_revision_locally(rev).await.ok().flatten(),
                };
                if head_now.is_some() && head_now == target_sha {
                    Ok(None)
                } else if let Some(rev) = effective_rev.as_deref().filter(|_| dst_path.exists()) {
                    // HEAD != target だけど clone はある → local-only checkout を試す。
                    // 成功 = commit が local DB にあって切替えできた (fetched=false のまま)。
                    // 失敗 = commit が local に無い → full flow か error。
                    match repo.checkout_locally(rev).await {
                        Ok(change) => Ok(change),
                        Err(e) => {
                            if hard_skip {
                                Err(anyhow::anyhow!(
                                    "{}: --no-refresh cannot be satisfied locally (rev '{}' not in local object DB: {})",
                                    plugin.display_name(),
                                    rev,
                                    e,
                                ))
                            } else {
                                fetched = true;
                                repo.sync().await
                            }
                        }
                    }
                } else if hard_skip {
                    Err(anyhow::anyhow!(
                        "{}: --no-refresh cannot be satisfied (plugin not cloned)",
                        plugin.display_name()
                    ))
                } else {
                    fetched = true;
                    repo.sync().await
                }
            };
            match res {
                Ok(change) => {
                    // build は HEAD が動いたとき (= fresh clone or pull で新 commit を
                    // 取得したとき) だけ実行する。no-op sync で毎回 build を回すのは
                    // 200+ プラグイン構成では体感で遅い。`--rebuild` で従来挙動 (常に
                    // 全 build プラグインで実行) に戻せるので、`:TSUpdate` 等を強制
                    // 走らせたいときの逃げ道は確保。
                    let has_any_build = plugin.build.is_some() || plugin.build_lua.is_some();
                    let should_build = has_any_build && (force_rebuild || change.is_some());
                    let build_warn = if should_build {
                        // status 表示は shell コマンドを優先 (具体的に何が走るか
                        // user が分かるから)。shell が無く lua のみなら "(lua build)"
                        // で済ます — Lua スニペット全文は冗長なので。
                        let label = match plugin.build.as_deref() {
                            Some(cmd) if plugin.build_lua.is_some() => format!("{cmd} + (lua)"),
                            Some(cmd) => cmd.to_string(),
                            None => "(lua build)".to_string(),
                        };
                        let _ = tx
                            .send((
                                plugin.url.clone(),
                                PluginStatus::Syncing(format!("Building: {label}")),
                            ))
                            .await;
                        execute_build_command(&plugin, &dst_path, &config_for_build, &cache_root)
                            .await
                    } else {
                        None
                    };
                    if let Some(ref err) = build_warn {
                        let _ = tx
                            .send((
                                plugin.url.clone(),
                                PluginStatus::Syncing(format!("Build warning: {}", err)),
                            ))
                            .await;
                    }
                    // lockfile 記録用に現在の HEAD commit を確定させる。
                    // GitChange は HEAD が動いた時のみ返されるので no-op sync でも
                    // lockfile エントリが新規作成できるよう別途取得する。
                    // 失敗しても sync 本体の成否には影響させない (resilience)。
                    let head_commit = repo.head_commit().await.ok();
                    // held-back 判定用に remote tracking tip を読む (HEAD は動かさない)。
                    // 失敗時は None → classify_held_back 側で「判定不能」扱いになり、
                    // 結果として held_back リストには積まれない (resilience)。
                    //
                    // fast-path (fetched = false) で分類すると local の remote tracking
                    // ref が stale なので、window 中に上流が動いたケースで false positive
                    // を出す。fast path では一律 None にして、window 超過で full fetch
                    // したタイミングで再評価する。
                    //
                    // 分類が None 確定の前提条件 (explicit rev / lockfile 寄与なし) では
                    // gix open 自体をスキップして 200+ プラグイン構成の I/O を減らす。
                    let remote_head = if fetched && plugin.rev.is_none() && effective_rev.is_some()
                    {
                        repo.remote_head().await.ok().flatten()
                    } else {
                        None
                    };
                    let held_back = if fetched {
                        classify_held_back(
                            plugin.rev.as_deref(),
                            effective_rev.as_deref(),
                            head_commit.as_deref(),
                            remote_head.as_deref(),
                        )
                        .map(|(pinned, remote)| HeldBackPin {
                            name: plugin.display_name(),
                            pinned,
                            remote,
                        })
                    } else {
                        None
                    };
                    // cooldown 観測 (#supply-chain): fetch を実行した plugin の remote
                    // tip を記録する。lockfile pin 中 (effective_rev 有り) は上の
                    // remote_head が tip。それ以外 (fresh clone / `--no-lock`) は sync
                    // が HEAD を tip に揃えた後なので head_commit == tip (gix open の
                    // 追加 I/O 無しで済む)。
                    let cooldown_entry: Option<crate::cooldown::CooldownEntry> =
                        match cooldown_ctx {
                            Some(mut ctx) if fetched => {
                                let tip = if effective_rev.is_some() {
                                    remote_head.clone()
                                } else {
                                    head_commit.clone()
                                };
                                match tip {
                                    Some(tip) => {
                                        let now = std::time::SystemTime::now();
                                        if !ctx.observed.iter().any(|o| o.commit == tip) {
                                            let committed =
                                                repo.commit_time(&tip).await.ok().flatten();
                                            crate::cooldown::observe(
                                                &mut ctx.observed,
                                                &tip,
                                                committed,
                                                now,
                                            );
                                        }
                                        crate::cooldown::prune(
                                            &mut ctx.observed,
                                            head_commit.as_deref(),
                                            now,
                                            ctx.cooldown,
                                        );
                                        Some(crate::cooldown::CooldownEntry {
                                            name: plugin.display_name(),
                                            url: plugin.url.clone(),
                                            observed: ctx.observed,
                                        })
                                    }
                                    None => None,
                                }
                            }
                            _ => None,
                        };
                    let _ = tx.send((plugin.url.clone(), PluginStatus::Finished)).await;
                    Ok((plugin, dst_path, build_warn, change, head_commit, held_back, fetched, cooldown_entry))
                }
                Err(e) => {
                    let _ = tx
                        .send((plugin.url.clone(), PluginStatus::Failed(e.to_string())))
                        .await;
                    Err(e)
                }
            }
        });
    }

    // 全タスクを spawn し終えたので元の tx を drop。
    // これにより全タスク完了後に rx が閉じ、channel のリークを防ぐ。
    drop(tx);

    // dev プラグインは sync しないが loader には含めるので先に scripts を作る
    let mut plugin_scripts = Vec::new();
    let mut merge_conflicts: Vec<crate::merge_conflicts::MergeConflictReport> = Vec::new();
    // merged/ 相対 path → 勝者 plugin 名。順次 merge しながら積み上げて、
    // 後続 plugin の衝突時に勝者を lookup するのに使う。
    let mut merge_ownership: std::collections::HashMap<PathBuf, String> =
        std::collections::HashMap::new();
    // 今回の sync で「期待される」view の集合。 sync 末尾の `prune_stale_views`
    // でこの集合に居ない `views/<plug>/` を削除する (#119)。
    //
    // **重要**: sync タスクの成功ブロック内で populate するのではなく、 config 上の
    // 全 plugin から事前計算する。 sync が失敗した (= 通信エラー等) plugin の
    // view を誤削除しない (resilience: 既存 loader.lua はそこを参照しているので、
    // 削除すると次回 Neovim 起動時に壊れる)。 Gemini Code Assist の指摘 (#120) を
    // 反映。
    let mut expected_views: std::collections::HashSet<PathBuf> = config
        .plugins
        .iter()
        .filter_map(|plugin| {
            let mode = decide_merge_mode(
                plugin.merge,
                plugin.lazy,
                plugin.merge_doc,
                config.options.merge_doc,
            );
            if matches!(
                mode,
                PluginMergeMode::ViewWithDoc | PluginMergeMode::ViewWithoutDoc
            ) {
                Some(resolve_plugin_view_dir(&views_dir, plugin))
            } else {
                None
            }
        })
        .collect();
    let config_root = resolve_config_root(config.options.config_root.as_deref());
    for plugin in config.plugins.iter().filter(|p| p.dev) {
        let dst_path = resolve_plugin_dst(plugin, &cache_root);
        let plugin_config_dir = resolve_plugin_config_dir(&config_root, plugin);
        let view_dir = resolve_plugin_view_dir(&views_dir, plugin);
        let mode = decide_merge_mode(
            plugin.merge,
            plugin.lazy,
            plugin.merge_doc,
            config.options.merge_doc,
        );
        // expected_views は config 全体から事前計算済 (上の collect)。 ここでは
        // dispatch のみ行う。 dev plugin は commit と無関係に中身が変わるので
        // stamp 判定はせず常に rebuild (expected_view_stamp の dev=true 経路)。
        dispatch_plugin_merge(
            mode,
            &dst_path,
            &merged_dir,
            &view_dir,
            &plugin.display_name(),
            None,
            false,
            &mut merge_ownership,
            &mut merge_conflicts,
        );
        plugin_scripts.push(build_plugin_scripts(
            plugin,
            &dst_path,
            &plugin_config_dir,
            &view_dir,
            mode,
        ));
    }

    let mut build_warnings: Vec<(String, String)> = Vec::new();
    let mut sync_changes: Vec<crate::update_log::ChangeRecord> = Vec::new();
    // lockfile pin で最新から置いてけぼりになっているプラグインを集めて、sync 末尾に
    // まとめて「`rvpm update` で進めて」と案内する (罠の緩和)。
    let mut held_back: Vec<HeldBackPin> = Vec::new();
    let mut finished_tasks = 0;
    let dev_count = config.plugins.iter().filter(|p| p.dev).count();
    let total_tasks = config.plugins.len() - dev_count;

    while finished_tasks < total_tasks {
        terminal.draw(|f| tui_state.draw(f, "syncing...", &icons))?;

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
                if let Ok(Ok((plugin, dst_path, build_warn, git_change, head_commit, pin, fetched, cooldown_entry))) = res {
                    if let Some(warn) = build_warn {
                        build_warnings.push((plugin.url.clone(), warn));
                    }
                    if let Some(change) = git_change {
                        sync_changes.push(change_record_from(&plugin, change));
                    }
                    if let Some(p) = pin {
                        held_back.push(p);
                    }
                    // cooldown 観測履歴の反映 (#supply-chain)。永続化は sync 末尾。
                    if let Some(entry) = cooldown_entry {
                        cooldown_state.upsert(entry);
                    }
                    // fetch を実行したプラグインだけ last_fetched を更新する。
                    // fast-path で素通ししたプラグインは元のタイムスタンプを保つ
                    // ので次回も window 判定に同じ起点を使う。
                    if fetched {
                        fetch_state.upsert(crate::fetch_state::FetchEntry {
                            name: plugin.display_name(),
                            url: plugin.url.clone(),
                            last_fetched: crate::fetch_state::now_rfc3339(),
                        });
                    }
                    // lockfile 更新: sync 完了後の現 HEAD を pin として記録。
                    // `--no-lock` 時もこの in-memory 更新は行うが、terminal save は飛ばすので
                    // ディスクには書かれない (lockfile 自体が default 空インスタンスのまま)。
                    if let Some(commit) = &head_commit {
                        lockfile.upsert(crate::lockfile::LockEntry {
                            name: plugin.display_name(),
                            url: plugin.url.clone(),
                            commit: commit.clone(),
                        });
                    }
                    // 統一案 (#119): MergeMode に従って merged/ または views/<plug>/ に
                    // tree を構築する。 Full → merged/ 全部、 ViewWithDoc → view 全部、
                    // ViewWithoutDoc → view (doc 抜き) + merged/doc/ に doc 集約。
                    let view_dir = resolve_plugin_view_dir(&views_dir, &plugin);
                    let mode = decide_merge_mode(
                        plugin.merge,
                        plugin.lazy,
                        plugin.merge_doc,
                        config.options.merge_doc,
                    );
                    // view stamp による incremental skip (#perf): HEAD が動いて
                    // いなければ view rebuild を省略。 `--rebuild` スコープ内は
                    // build hook が clone 内へ artifact を吐き直すため強制 rebuild。
                    let force_view = matches_rebuild_filter(&plugin, rebuild_filter_lc.as_deref());
                    let stamp = expected_view_stamp(mode, head_commit.as_deref(), plugin.dev);
                    // expected_views は run_sync 冒頭で config 全体から事前計算済。
                    dispatch_plugin_merge(
                        mode,
                        &dst_path,
                        &merged_dir,
                        &view_dir,
                        &plugin.display_name(),
                        stamp.as_ref(),
                        force_view,
                        &mut merge_ownership,
                        &mut merge_conflicts,
                    );
                    let config_root = resolve_config_root(config.options.config_root.as_deref());
                    let plugin_config_dir = resolve_plugin_config_dir(&config_root, &plugin);
                    let scripts = build_plugin_scripts(
                        &plugin,
                        &dst_path,
                        &plugin_config_dir,
                        &view_dir,
                        mode,
                    );
                    plugin_scripts.push(scripts);
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }

    // JoinSet は完了順で返すので plugin_scripts が依存順になっていない。
    // config.plugins の順序 (sort_plugins 済み) に合わせて re-sort する。
    plugin_scripts.sort_by_key(|ps| {
        config
            .plugins
            .iter()
            .position(|p| p.display_name() == ps.name)
            .unwrap_or(usize::MAX)
    });

    // lazy → eager 昇格後に merge が必要なプラグインを追加で merge する。
    // sync 時点では lazy のため merge されなかったが、depends/on_source により
    // eager に昇格されるプラグインは merged/ にリンクが必要。
    let promoted = crate::loader::promote_lazy_to_eager(&mut plugin_scripts);
    if !promoted.is_empty() {
        for ps in &mut plugin_scripts {
            if promoted.contains(&ps.name) && ps.merge {
                let dst = PathBuf::from(&ps.path);
                // 昇格された plugin は eager + merge=true 扱い → Full merge。
                // sync 一巡目で `views/<plug>/` (場合により merged/doc/ にも) に
                // 配置済みの可能性あり。 自己 conflict は record_merge_result が
                // フィルタするので false-positive にはならない。
                //
                // PluginScripts.view_path は build_plugin_scripts で正しく解決済の
                // `views/<host>/<owner>/<repo>/`。 fragile な path 再構築を経由しない。
                let view_dir = PathBuf::from(&ps.view_path);
                dispatch_plugin_merge(
                    PluginMergeMode::Full,
                    &dst,
                    &merged_dir,
                    &view_dir,
                    &ps.name,
                    None,
                    false,
                    &mut merge_ownership,
                    &mut merge_conflicts,
                );
                // promoted 後は Full merge → views は不要。 sync 末尾の
                // `prune_stale_views` で削除されるよう expected_views から除外
                // (もう view を期待しない)。 ついでに ps.view_path も merged/ に
                // 切り替える: load_lazy 側はもう呼ばれないが、 phase 6 の
                // eager 経路で `vim.opt.rtp:append` が走らないこと (= merge=true
                // なので skip される) を担保するため、 view_path は使われないが
                // 念のため `merged_dir` に統一しておく。
                expected_views.remove(&view_dir);
                ps.view_path = merged_dir.to_string_lossy().replace('\\', "/");
            }
        }
    }

    // 統一案 (#119): 期待されない views/<plug>/ を sweep。
    // - config から削除された plugin の view
    // - promote_lazy_to_eager で View → Full に切り替わった plugin の view
    // 両方が一括で消える。
    prune_stale_views(&views_dir, &expected_views);

    terminal.draw(|f| tui_state.draw(f, "syncing...", &icons))?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    // TUI cleanup — 各ステップが失敗しても次を続行してターミナルを確実に復元する
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    // sync 結果のサマリーを出力 (TUI 閉じた後なので見える)
    // plugins 順で出力して決定的な順序を保つ
    let failed: Vec<_> = tui_state
        .plugins
        .iter()
        .filter_map(|url| match tui_state.status_map.get(url) {
            Some(PluginStatus::Failed(msg)) => Some((url.as_str(), msg.as_str())),
            _ => None,
        })
        .collect();
    if !failed.is_empty() {
        eprintln!("\n{} error(s):", failed.len());
        for (url, msg) in &failed {
            eprintln!("  \u{2717} {}: {}", url, msg);
        }
    }
    if !build_warnings.is_empty() {
        eprintln!("\n{} build warning(s):", build_warnings.len());
        for (url, msg) in &build_warnings {
            eprintln!("  \u{26a0} {}: {}", url, msg);
        }
    }
    if !promoted.is_empty() {
        let mut sorted_promoted: Vec<_> = promoted.iter().collect();
        sorted_promoted.sort();
        eprintln!("\n{} plugin(s) promoted lazy -> eager:", promoted.len());
        for name in &sorted_promoted {
            eprintln!("  -> {}", name);
        }
    }
    // lockfile pin で最新から置いてけぼりのプラグインを案内する。`rvpm sync` は
    // pin を尊重する (= update しない) 設計なので、rev を設定していないユーザーが
    // 「sync したのに古いまま」と感じる罠を緩和する目的。
    if !held_back.is_empty() {
        held_back.sort_by(|a, b| a.name.cmp(&b.name));
        eprintln!(
            "\n{} plugin(s) held at lockfile pin (no `rev` set). Run `rvpm update` to advance:",
            held_back.len()
        );
        for pin in &held_back {
            eprintln!(
                "  -> {}  {} -> {}",
                pin.name,
                crate::update_log::short_hash(&pin.pinned),
                crate::update_log::short_hash(&pin.remote),
            );
        }
    }
    println!("Generating loader.lua...");
    let loader_path = resolve_loader_path(&cache_root);
    write_loader_to_path(
        &merged_dir,
        &plugin_scripts,
        &loader_path,
        &build_loader_options(&config_root),
    )?;
    println!("Done! -> {}", loader_path.display());

    if config.options.auto_helptags {
        println!("Generating helptags...");
        let report = crate::helptags::build_helptags(&plugin_scripts, &merged_dir).await?;
        match (report.ran, report.exit_code) {
            (true, Some(0)) => {
                println!(
                    "Done! helptags built for {} doc director(y/ies)",
                    report.target_count
                );
            }
            (true, Some(code)) => {
                eprintln!(
                    "\u{26a0} helptags: nvim exited with code {} ({} target(s) attempted)",
                    code, report.target_count
                );
            }
            (true, None) => {
                eprintln!(
                    "\u{26a0} helptags: nvim terminated without exit code ({} target(s) attempted)",
                    report.target_count
                );
            }
            (false, _) => {
                // build_helptags 内で warn を流済み (nvim 不在 / target 0)。
            }
        }
    }

    // 未使用 plugin ディレクトリの処理:
    //  - `--prune` フラグまたは `options.auto_clean = true` で自動削除
    //  - それ以外なら警告のみ (rvpm clean で後処理できる旨を案内)
    let force = prune || config.options.auto_clean;
    let (count, unused) = maybe_prune_unused_repos(&config, &cache_root, force);
    if !force && count > 0 {
        println!();
        println!(
            "\u{26a0} Found {} unused plugin {}:",
            count,
            plural("directory", "directories", count),
        );
        for path in &unused {
            println!("    {}", path.display());
        }
        println!(
            "  Run `rvpm clean` (fast, no git) or `rvpm sync --prune` to delete them,\n  \
             or set `auto_clean = true` under `[options]` to do it automatically."
        );
    }

    print_merge_conflicts(&merge_conflicts);
    // 直近 sync の衝突 snapshot を atomic write (empty でも上書き)。
    // doctor が読む。失敗しても本処理は止めない (resilience)。
    let mc_path = resolve_merge_conflicts_path(&cache_root);
    if let Err(e) = crate::merge_conflicts::save_snapshot(&mc_path, merge_conflicts.clone()) {
        eprintln!(
            "\u{26a0} failed to save {}: {} (doctor state may be stale)",
            mc_path.display(),
            e
        );
    }

    // 並列 spawn 完了順で積まれているので plugin 名で安定 sort。
    // `rvpm log` の出力が同じ sync 結果に対して常に同じになる。
    sync_changes.sort_by(|a, b| a.name.cmp(&b.name));
    record_changes_or_warn(&cache_root, "sync", sync_changes);

    // lockfile save: config.toml から外されたプラグインの entry を drop してから
    // atomic write。`--no-lock` 時は完全スキップ (ディスクに触らない = 既存の
    // lockfile がユーザーの dotfile にあってもそのまま保つ)。
    // `options.chezmoi = true` なら source 側に書いて `chezmoi apply` で target
    // へ反映する (config.toml / hook ファイルと同じ流儀)。
    if !no_lock {
        let active: std::collections::HashSet<String> = config
            .plugins
            .iter()
            .filter(|p| !p.dev)
            .map(|p| p.display_name())
            .collect();
        lockfile.retain_by_names(&active);
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
    }

    // fetch cache の永続化は lockfile とは独立。`--no-lock` でも保存する
    // (ephemeral cache は dotfile 管理の reproducibility と無関係で、ユーザー
    // マシン単位のローカル最適化だけの話なので)。config.toml から外された
    // プラグインの entry は drop する。
    let active_names: std::collections::HashSet<String> = config
        .plugins
        .iter()
        .filter(|p| !p.dev)
        .map(|p| p.display_name())
        .collect();
    fetch_state.retain_by_names(&active_names);
    if let Err(e) = fetch_state.save(&fetch_state_path) {
        eprintln!(
            "\u{26a0} failed to save {}: {} (fetch cache may be stale)",
            fetch_state_path.display(),
            e
        );
    }

    // cooldown 観測履歴の永続化 (#supply-chain)。cooldown を使っている config の
    // ときだけディスクに触る。config.toml から外れた plugin の entry は drop。
    if cooldown_tracking_any {
        cooldown_state.retain_by_names(&active_names);
        if let Err(e) = cooldown_state.save(&cooldown_state_path) {
            eprintln!(
                "\u{26a0} failed to save {}: {} (cooldown observations may be stale)",
                cooldown_state_path.display(),
                e
            );
        }
    }

    print_init_lua_hint_if_missing(&config);
    Ok(())
}

/// A plugin that sync left at a lockfile-pinned commit while its remote
/// has advanced. Reported in the sync summary so users know `rvpm update`
/// would actually change something.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HeldBackPin {
    name: String,
    pinned: String,
    remote: String,
}

/// Decide whether a just-synced plugin is being "held back" by a lockfile
/// pin. Returns `Some((pinned_commit, remote_tip))` when:
///
/// - the plugin has **no explicit `rev`** (not the user's choice),
/// - the **lockfile contributed** an effective rev (pin came from the lockfile),
/// - and the pinned HEAD is **behind the remote tracking tip**.
///
/// Returns `None` otherwise — explicit `rev`, no lockfile contribution,
/// already at remote tip, or either commit unknown (treat unknowns as
/// "not held back" to avoid false positives).
///
/// This is the pure classification step; the caller is responsible for
/// composing the plugin display name and emitting the summary line.
fn classify_held_back(
    plugin_rev: Option<&str>,
    effective_rev: Option<&str>,
    head_commit: Option<&str>,
    remote_head: Option<&str>,
) -> Option<(String, String)> {
    if plugin_rev.is_some() {
        return None;
    }
    effective_rev?;
    let head = head_commit?;
    let remote = remote_head?;
    if head != remote {
        Some((head.to_string(), remote.to_string()))
    } else {
        None
    }
}

/// 現在の plugin が `--rebuild [QUERY]` のスコープに入るか判定する。
///
/// `rebuild_filter_lc` は **呼び出し側で小文字化済み** の前提。`run_sync` は N
/// プラグインを回すので、N 回同じ query を lowercase するコストを避けるため
/// 事前正規化を外側に持たせている。
///
/// - `None` (フラグ未指定) → 常に false (build 強制しない)
/// - `Some("")` (フラグのみ、値無し) → 常に true (全 plugin)
/// - `Some(q)` (フラグ + query) → plugin の url / name の
///   いずれかに q が含まれれば true。url 側で決着すれば name の lowercase
///   allocation は走らない (短絡評価)。
fn matches_rebuild_filter(plugin: &crate::config::Plugin, rebuild_filter_lc: Option<&str>) -> bool {
    match rebuild_filter_lc {
        None => false,
        Some("") => true,
        Some(qlc) => {
            plugin.url.to_ascii_lowercase().contains(qlc)
                || plugin
                    .name
                    .as_deref()
                    .is_some_and(|n| n.to_ascii_lowercase().contains(qlc))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Plugin;

    // ─── classify_held_back ──────────────────────────────────────────────
    // Pure classification of "this plugin is being held back by a lockfile
    // pin". The integration path is covered by the git::remote_head test
    // on the git.rs side; these tests nail down the decision table only.

    #[test]
    fn test_classify_held_back_reports_pin_behind_remote() {
        // No rev set, lockfile contributed a commit, and HEAD (= pin)
        // lags behind remote → this is the case users hit.
        let got = classify_held_back(
            None,
            Some("lockcommit"),
            Some("lockcommit"),
            Some("newcommit"),
        );
        assert_eq!(
            got,
            Some(("lockcommit".to_string(), "newcommit".to_string()))
        );
    }

    #[test]
    fn test_classify_held_back_silent_when_explicit_rev_set() {
        // If the user pinned via `rev`, the lag is intentional — not our
        // call to flag.
        let got = classify_held_back(Some("v1.0.0"), Some("v1.0.0"), Some("abc"), Some("def"));
        assert_eq!(got, None);
    }

    #[test]
    fn test_classify_held_back_silent_without_lockfile_contribution() {
        // No lockfile entry → sync already reset to remote; there's no pin
        // holding us back.
        let got = classify_held_back(None, None, Some("abc"), Some("abc"));
        assert_eq!(got, None);
    }

    #[test]
    fn test_classify_held_back_silent_when_pin_matches_remote() {
        // Lockfile pin and remote happen to line up — everyone's up to
        // date, no reason to nag.
        let got = classify_held_back(None, Some("abc"), Some("abc"), Some("abc"));
        assert_eq!(got, None);
    }

    #[test]
    fn test_classify_held_back_silent_when_remote_head_unknown() {
        // Can't prove the pin is behind if we can't resolve the remote tip.
        // Prefer silence over a false positive (resilience).
        let got = classify_held_back(None, Some("abc"), Some("abc"), None);
        assert_eq!(got, None);
    }

    #[test]
    fn test_classify_held_back_silent_when_head_unknown() {
        // Same resilience argument in the other direction.
        let got = classify_held_back(None, Some("abc"), None, Some("def"));
        assert_eq!(got, None);
    }

    // ─── matches_rebuild_filter ─────────────────────────────────────────
    // `--rebuild [QUERY]` のスコープ判定を 3 分岐で押さえる:
    //   None        → 常に false (フラグ未指定、従来デフォルト)
    //   Some("")    → 常に true (フラグだけ、従来の `--rebuild` 挙動)
    //   Some("q")   → url / name に q を含めば true

    fn mk_plugin(url: &str, name: Option<&str>) -> Plugin {
        Plugin {
            url: url.to_string(),
            name: name.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_rebuild_filter_none_never_matches() {
        let p = mk_plugin("nvim-treesitter/nvim-treesitter", None);
        assert!(!matches_rebuild_filter(&p, None));
    }

    #[test]
    fn test_rebuild_filter_empty_always_matches() {
        let p = mk_plugin("folke/snacks.nvim", None);
        assert!(matches_rebuild_filter(&p, Some("")));
    }

    #[test]
    fn test_rebuild_filter_substring_matches_url() {
        // 呼び出し側で lowercase 済みの query を渡す契約。
        let p = mk_plugin("nvim-treesitter/nvim-treesitter", None);
        assert!(matches_rebuild_filter(&p, Some("treesitter")));
        assert!(!matches_rebuild_filter(&p, Some("telescope")));
    }

    #[test]
    fn test_rebuild_filter_requires_caller_to_lowercase_query() {
        // 契約: caller が lowercase してから渡す。大文字混じりは一致しない
        // (run_sync 側で事前正規化する理由)。
        let p = mk_plugin("nvim-treesitter/nvim-treesitter", None);
        assert!(
            !matches_rebuild_filter(&p, Some("TREESITTER")),
            "case-insensitivity is the caller's responsibility"
        );
    }

    #[test]
    fn test_rebuild_filter_matches_explicit_name() {
        // URL と name が独立: name 側で hit させたいケース
        let p = mk_plugin("owner/repo", Some("my-alias"));
        assert!(matches_rebuild_filter(&p, Some("alias")));
    }

    #[test]
    fn test_rebuild_filter_no_false_match_without_name() {
        // name = None のとき、url にだけマッチングが走る (name 側で空文字との
        // 意図せぬ contains が起きないこと)
        let p = mk_plugin("foo/bar", None);
        assert!(!matches_rebuild_filter(&p, Some("baz")));
    }
}
