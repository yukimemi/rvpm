use super::*;

pub(crate) async fn run_generate(force: bool) -> Result<()> {
    // early return (`?`) でも背景削除スレッドを必ず回収する (Gemini PR #229)。
    let _reap_guard = ReapGuard;
    let timing = std::env::var_os("RVPM_TIMING").is_some();
    let t0 = std::time::Instant::now();
    let mut t_prev = t0;
    let lap = |label: &str, t_prev: &mut std::time::Instant| {
        if timing {
            eprintln!(
                "[timing] {:<24} {:>8.3}s (total {:>7.3}s)",
                label,
                t_prev.elapsed().as_secs_f64(),
                t0.elapsed().as_secs_f64()
            );
        }
        *t_prev = std::time::Instant::now();
    };
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
    let mut config = parse_config(&toml_content)?;
    crate::config::sort_plugins(&mut config.plugins)?;
    for plugin in config.plugins.iter_mut() {
        disable_merge_if_cond(plugin);
    }
    let cache_root = resolve_cache_root(config.options.cache_root.as_deref());
    let merged_dir = resolve_merged_dir(&cache_root);
    let views_dir = resolve_views_dir(&cache_root);
    let loader_path = resolve_loader_path(&cache_root);

    lap("config parse+sort", &mut t_prev);
    // ── PluginScripts + HEAD commit を並列収集 (#perf) ──
    // `build_plugin_scripts` は pre-glob に加えて plugin_scan が lua/ 以下の全
    // ソース内容を読む I/O heavy な処理 (200+ plugin 構成で 1s 超)。 stamp 用の
    // gix HEAD 読みも plugin 数ぶん積もる。 sync と同じ concurrency 上限で
    // spawn_blocking に逃がし、 config 順 (sort_plugins 済) を index で保って
    // collect する。
    let config = Arc::new(config);
    let config_root = resolve_config_root(config.options.config_root.as_deref());
    let concurrency = resolve_concurrency(config.options.concurrency);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut script_set: JoinSet<(usize, crate::loader::PluginScripts, Option<String>)> =
        JoinSet::new();
    for (idx, plugin) in config.plugins.iter().enumerate() {
        let plugin = plugin.clone();
        let cache_root = cache_root.clone();
        let views_dir = views_dir.clone();
        let config_root = config_root.clone();
        let merge_doc_default = config.options.merge_doc;
        let sem = Arc::clone(&semaphore);
        script_set.spawn(async move {
            let _permit = sem.acquire_owned().await;
            tokio::task::spawn_blocking(move || {
                let dst_path = resolve_plugin_dst(&plugin, &cache_root);
                let plugin_config_dir = resolve_plugin_config_dir(&config_root, &plugin);
                let view_dir = resolve_plugin_view_dir(&views_dir, &plugin);
                let mode = decide_merge_mode(
                    plugin.merge,
                    plugin.lazy,
                    plugin.merge_doc,
                    merge_doc_default,
                );
                let scripts =
                    build_plugin_scripts(&plugin, &dst_path, &plugin_config_dir, &view_dir, mode);
                // view stamp fingerprint 用の HEAD commit。 dev plugin は commit と
                // 無関係に中身が変わるので読まない (= 常に rebuild)。 .git が無い /
                // 読めない clone も None → 同じく安全側で毎回 rebuild。
                let commit = if plugin.dev {
                    None
                } else {
                    crate::git::head_commit_of(&dst_path).ok()
                };
                (idx, scripts, commit)
            })
            .await
            .expect("build_plugin_scripts task panicked")
        });
    }
    let mut indexed: Vec<Option<(crate::loader::PluginScripts, Option<String>)>> =
        (0..config.plugins.len()).map(|_| None).collect();
    while let Some(res) = script_set.join_next().await {
        let (idx, scripts, commit) = res?;
        indexed[idx] = Some((scripts, commit));
    }
    let mut plugin_scripts = Vec::with_capacity(indexed.len());
    // plugin_scripts と同 index で並ぶ HEAD commit (stamp fingerprint 用)。
    let mut commits: Vec<Option<String>> = Vec::with_capacity(indexed.len());
    for entry in indexed.into_iter().flatten() {
        plugin_scripts.push(entry.0);
        commits.push(entry.1);
    }

    lap("build_plugin_scripts", &mut t_prev);
    // lazy → eager 昇格を適用。
    // **merged/ も views/<plug>/ も wholesale 削除しない** (#129 CodeRabbit 指摘):
    // Neovim が走ってる状態で `rvpm generate` が動いた瞬間に Full plugin の lua
    // module require / merged/doc 経由の `:help` lookup / lazy plugin の load_lazy が
    // 走ると、 wipe → relink の間にファイルが消えて race するため。
    // 対策: per-plugin view は `atomic_replace_view_dir` で個別 atomic 置換、
    // 共有 merged/ は build ループ全体を `atomic_replace_view_dir(merged_dir, ...)`
    // で囲んで全 plugin 分の hard-link を tmp dir に積んでから atomic rename する。
    crate::loader::promote_lazy_to_eager(&mut plugin_scripts);
    std::fs::create_dir_all(&views_dir)?;
    let mut merge_conflicts: Vec<crate::merge_conflicts::MergeConflictReport> = Vec::new();

    // ── Phase V: per-plugin view を並列 build (#perf) ──
    // views/<plug>/ は per-plugin に独立で互いに衝突しないので、 sync の git
    // 操作と同じ concurrency 上限で並列化できる。 さらに stamp (clone の HEAD
    // commit + merge mode) が前回 build と一致する plugin は walk + hard-link
    // を丸ごと skip する — hard link は inode 共有なのでファイル内容は clone に
    // 自動追従し、 変わり得る「ファイル集合」は commit が動いた時だけ。
    let mut views_rebuilt = 0usize;
    {
        let mut view_set: JoinSet<(Vec<crate::merge_conflicts::MergeConflictReport>, bool)> =
            JoinSet::new();
        for (idx, ps) in plugin_scripts.iter().enumerate() {
            let mode = decide_merge_mode(ps.merge, ps.lazy, ps.merge_doc, config.options.merge_doc);
            let merge_fn: fn(&Path, &Path) -> anyhow::Result<crate::link::MergeResult> = match mode
            {
                PluginMergeMode::ViewWithDoc => crate::link::merge_plugin_view,
                PluginMergeMode::ViewWithoutDoc => crate::link::merge_plugin_view_no_doc,
                PluginMergeMode::Full => continue,
            };
            let dst = PathBuf::from(&ps.path);
            if !dst.exists() {
                continue;
            }
            let view_dir = PathBuf::from(&ps.view_path);
            let name = ps.name.clone();
            let stamp = expected_view_stamp(mode, commits[idx].as_deref(), ps.dev);
            let sem = Arc::clone(&semaphore);
            view_set.spawn(async move {
                let _permit = sem.acquire_owned().await;
                tokio::task::spawn_blocking(move || {
                    let mut conflicts = Vec::new();
                    let built = build_view_if_needed(
                        &dst,
                        &view_dir,
                        &name,
                        stamp.as_ref(),
                        force,
                        &mut conflicts,
                        merge_fn,
                    );
                    (conflicts, built)
                })
                .await
                .expect("view build task panicked")
            });
        }
        while let Some(res) = view_set.join_next().await {
            let (conflicts, built) = res?;
            merge_conflicts.extend(conflicts);
            if built {
                views_rebuilt += 1;
            }
        }
    }
    lap("view builds", &mut t_prev);

    // ── Phase M: merged/ を構築 (Full 全 rtp dir + ViewWithoutDoc の doc/) ──
    // first-wins の勝敗が処理順に依存するため、 ここは config 順の逐次のまま。
    // 寄与 plugin 全員の (name, commit, 寄与種別) を結合した stamp が前回と
    // 一致すれば、 merged/ の rebuild も丸ごと skip する。 1 plugin でも commit
    // 不明 (dev / 非 git clone) なら skip 判定はせず毎回 rebuild (安全側)。
    let mut merge_ownership: std::collections::HashMap<PathBuf, String> =
        std::collections::HashMap::new();
    let mut merged_parts: Vec<(String, String, &'static str)> = Vec::new();
    let mut merged_skippable = true;
    let mut contributors: Vec<(usize, PluginMergeMode)> = Vec::new();
    for (idx, ps) in plugin_scripts.iter().enumerate() {
        let dst = PathBuf::from(&ps.path);
        if !dst.exists() {
            continue;
        }
        let mode = decide_merge_mode(ps.merge, ps.lazy, ps.merge_doc, config.options.merge_doc);
        let kind = match mode {
            PluginMergeMode::Full => "full",
            PluginMergeMode::ViewWithoutDoc => "doc",
            PluginMergeMode::ViewWithDoc => continue,
        };
        match &commits[idx] {
            Some(c) if !ps.dev => merged_parts.push((ps.name.clone(), c.clone(), kind)),
            _ => merged_skippable = false,
        }
        contributors.push((idx, mode));
    }
    let merged_stamp = merged_skippable.then(|| {
        crate::view_stamp::ViewStamp::new(crate::view_stamp::merged_fingerprint(&merged_parts))
    });
    let merged_skipped = !force
        && merged_stamp
            .as_ref()
            .is_some_and(|s| crate::view_stamp::is_current(&merged_dir, s));
    if !merged_skipped {
        let merged_atomic_res = atomic_replace_view_dir(&merged_dir, |tmp_merged| {
            std::fs::create_dir_all(tmp_merged)?;
            for (idx, mode) in &contributors {
                let ps = &plugin_scripts[*idx];
                let dst = PathBuf::from(&ps.path);
                match mode {
                    PluginMergeMode::Full => {
                        let r = crate::link::merge_plugin(&dst, tmp_merged);
                        record_merge_result(
                            &ps.name,
                            r,
                            &mut merge_ownership,
                            &mut merge_conflicts,
                        );
                    }
                    PluginMergeMode::ViewWithoutDoc => {
                        let r = crate::link::merge_plugin_doc_only(&dst, tmp_merged);
                        record_merge_result(
                            &ps.name,
                            r,
                            &mut merge_ownership,
                            &mut merge_conflicts,
                        );
                    }
                    PluginMergeMode::ViewWithDoc => {}
                }
            }
            if let Some(s) = &merged_stamp
                && let Err(e) = crate::view_stamp::write(tmp_merged, s)
            {
                // stamp が書けなくても merged 自体は有効 — 次回 rebuild に倒れるだけ。
                eprintln!("\u{26a0} failed to write merged stamp: {}", e);
            }
            Ok(())
        });
        if let Err(e) = merged_atomic_res {
            eprintln!(
                "\u{26a0} atomic merged/ replace failed: {} (falling back to direct write)",
                e
            );
        }
    }
    lap("merge dispatch", &mut t_prev);

    // config から消えた plugin の views/<plug>/ を掃除 (CodeRabbit PR #129)。
    // sync 末尾でも同等の処理が走るが、 generate 単独実行時 (rvpm list TUI で
    // `c` 編集等) でも orphaned view を即座に sweep するために重複起動。
    let expected_views: std::collections::HashSet<PathBuf> = plugin_scripts
        .iter()
        .filter_map(|ps| {
            let mode = decide_merge_mode(ps.merge, ps.lazy, ps.merge_doc, config.options.merge_doc);
            if matches!(
                mode,
                PluginMergeMode::ViewWithDoc | PluginMergeMode::ViewWithoutDoc
            ) {
                Some(PathBuf::from(&ps.view_path))
            } else {
                None
            }
        })
        .collect();
    prune_stale_views(&views_dir, &expected_views);
    lap("prune_stale_views", &mut t_prev);

    println!("Generating loader.lua...");
    write_loader_to_path(
        &merged_dir,
        &plugin_scripts,
        &loader_path,
        &build_loader_options(&config_root),
    )?;
    println!("Done! -> {}", loader_path.display());
    lap("write_loader", &mut t_prev);

    if config.options.auto_helptags {
        // merged も全 view も今回触っていないなら既存 tags は最新のまま —
        // `nvim --headless` の起動を丸ごと省略する (#perf)。 tags が物理的に
        // 欠けているターゲットが 1 つでもあれば (前回 run が helptags 前に
        // 中断した等)、 通常経路に戻して生成し直す (resilience)。
        let helptags_current = merged_skipped && views_rebuilt == 0 && {
            let targets = crate::helptags::collect_helptag_targets(&plugin_scripts, &merged_dir);
            targets.iter().all(|doc| doc.join("tags").is_file())
        };
        if helptags_current {
            println!("helptags up-to-date (skipped)");
        } else {
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
    }
    lap("helptags", &mut t_prev);

    // `options.auto_clean = true` なら config から外されたプラグインディレクトリも
    // 自動削除 (git 操作は行わないので generate 自体のコストは増えない)。
    if config.options.auto_clean {
        let _ = maybe_prune_unused_repos(&config, &cache_root, true);
    }

    // merged/ を skip した run は first-wins の衝突計算自体が走っていないので、
    // snapshot を上書きすると doctor が「衝突ゼロ」と誤認する。 merged の中身は
    // 前回から変わっていない = 前回 snapshot がそのまま正なので温存する。
    if merged_skipped {
        if !merge_conflicts.is_empty() {
            // view 側 (per-plugin tree 内) の衝突だけは今回分を表示する。
            print_merge_conflicts(&merge_conflicts);
        }
    } else {
        print_merge_conflicts(&merge_conflicts);
        // 直近 generate の衝突 snapshot を保存 (sync と同じ扱い)。
        let mc_path = resolve_merge_conflicts_path(&cache_root);
        if let Err(e) = crate::merge_conflicts::save_snapshot(&mc_path, merge_conflicts.clone()) {
            eprintln!(
                "\u{26a0} failed to save {}: {} (doctor state may be stale)",
                mc_path.display(),
                e
            );
        }
    }
    // バックグラウンドへ逃がした旧 view dir (.rvpm-old) の削除は冒頭の
    // `_reap_guard` (Drop) が回収する — early return 経路も含めて漏れない。
    // loader / helptags の間に大半は終わっているので通常は一瞬。
    print_init_lua_hint_if_missing(&config);
    Ok(())
}
