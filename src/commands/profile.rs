use super::*;

/// `rvpm profile` 本体。
///
/// 流れ:
///   1. 前回 crash の `.bak` 検出 → 自動復元
///   2. `--no-instrument` で無ければ loader.lua を退避 + instrumented loader に差し替え
///   3. marker 空 .vim を tmp dir に事前作成
///   4. `nvim --headless --startuptime` を N 回実行
///   5. commit で原本復元、marker dir 削除
///   6. TUI / plain / JSON で出力
pub(crate) async fn run_profile(
    runs: usize,
    top: Option<usize>,
    json: bool,
    no_tui: bool,
    no_merge: bool,
    no_instrument: bool,
) -> Result<()> {
    let runs = runs.clamp(1, 20);

    // `--no-merge` は loader 側の force_unmerge に乗せる設計なので、instrumented
    // 版の loader.lua が生成されないと効かない。silent に無視すると「計測結果が
    // merged のままなのに no_merge=true で表示される」矛盾になるので fail fast。
    if no_merge && no_instrument {
        anyhow::bail!(
            "--no-merge requires loader instrumentation (it is applied at generate time); remove --no-instrument to use --no-merge"
        );
    }

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
    let loader_path = resolve_loader_path(&cache_root);
    // [user config] グループに入れるパス。rvpm 側 (`~/.config/rvpm/<appname>`) と
    // Neovim 側 (`~/.config/<appname>/`) 両方を候補にして、init.lua が [runtime] に
    // 落ちないようにする。
    let rvpm_config_root = resolve_config_root(config.options.config_root.as_deref());
    let nvim_config_root = nvim_init_lua_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let user_config_roots: Vec<std::path::PathBuf> = [rvpm_config_root, nvim_config_root]
        .into_iter()
        .filter(|p| !p.as_os_str().is_empty())
        .collect();

    recover_stale_loader_backup(&loader_path);

    // PluginPathEntry を組み立てる (profile.rs の path 帰属判定に渡す)
    let plugin_entries: Vec<crate::profile::PluginPathEntry> = config
        .plugins
        .iter()
        .map(|p| {
            let root = resolve_plugin_dst(p, &cache_root)
                .to_string_lossy()
                .to_string();
            crate::profile::PluginPathEntry {
                name: p.display_name(),
                root,
                lazy: p.lazy,
            }
        })
        .collect();

    // instrumented loader を書き出す (no_instrument 時は skip)。
    // marker_dir は `tempfile::TempDir` で取る — panic / early return / Ctrl-C で
    // 自動削除されるので、手動 remove_dir_all の漏れで tmp が汚染されない。
    let mut marker_dir_guard: Option<tempfile::TempDir> = None;
    let mut marker_dir: Option<PathBuf> = None;
    let mut swap_guard: Option<LoaderSwapGuard> = None;

    if !no_instrument {
        let tmp = tempfile::Builder::new()
            .prefix("rvpm-profile-markers-")
            .tempdir()
            .context("failed to create marker dir")?;
        let tmp_path = tmp.path().to_path_buf();

        let config_root = resolve_config_root(config.options.config_root.as_deref());
        let views_dir = resolve_views_dir(&cache_root);
        let mut plugin_scripts = Vec::new();
        for plugin in &config.plugins {
            let dst_path = resolve_plugin_dst(plugin, &cache_root);
            let plugin_config_dir = resolve_plugin_config_dir(&config_root, plugin);
            let view_dir = resolve_plugin_view_dir(&views_dir, plugin);
            let mode = decide_merge_mode(
                plugin.merge,
                plugin.lazy,
                plugin.merge_doc,
                config.options.merge_doc,
            );
            plugin_scripts.push(build_plugin_scripts(
                plugin,
                &dst_path,
                &plugin_config_dir,
                &view_dir,
                mode,
            ));
        }
        crate::loader::promote_lazy_to_eager(&mut plugin_scripts);

        // marker .vim 空ファイルを事前作成
        let expected = crate::loader::expected_markers(&plugin_scripts);
        for name in &expected {
            let f = tmp_path.join(format!("{}.vim", name));
            std::fs::write(&f, b"")
                .with_context(|| format!("failed to create marker {}", f.display()))?;
        }

        let guard = LoaderSwapGuard::create(loader_path.clone())?;
        let profile_opts = crate::loader::ProfileOptions {
            marker_dir: tmp_path.to_string_lossy().replace('\\', "/"),
            force_unmerge: no_merge,
        };
        let mut loader_opts = build_loader_options(&config_root);
        loader_opts.profile = Some(profile_opts);
        write_loader_to_path(&merged_dir, &plugin_scripts, &loader_path, &loader_opts)?;
        swap_guard = Some(guard);
        marker_dir = Some(tmp_path);
        marker_dir_guard = Some(tmp);
    }

    if !json && !no_tui {
        let mode = if no_instrument {
            "raw --startuptime"
        } else if no_merge {
            "instrumented + no-merge"
        } else {
            "instrumented"
        };
        eprintln!(
            "\u{26a1} rvpm profile: measuring nvim startup ({} run{}, {})…",
            runs,
            if runs == 1 { "" } else { "s" },
            mode
        );
    }

    let cfg = crate::profile::ProfileRunConfig {
        runs,
        plugins: plugin_entries,
        merged_dir,
        loader_path: loader_path.clone(),
        user_config_roots,
        marker_dir: marker_dir.clone(),
        no_merge,
        no_instrument,
    };

    // Ctrl-C 対応: profile 実行中にユーザが中断したら、Drop を待たず明示的に
    // swap_guard を commit して loader.lua を戻す。tokio::signal::ctrl_c は
    // SIGINT を tokio runtime 内で捕まえるため、panic!=unwind にならない環境
    // (release --abort=abort) でも Drop が走らないので、この手動 cleanup が必要。
    let report = tokio::select! {
        res = crate::profile::run_profile(cfg) => {
            res.context("profile run failed")?
        }
        _ = tokio::signal::ctrl_c() => {
            if let Some(g) = swap_guard.take() {
                let _ = g.commit();
            }
            drop(marker_dir_guard.take());
            anyhow::bail!("profile interrupted (Ctrl-C)");
        }
    };

    // 計測完了 → 原本復元を手動で commit (drop より前に明示的に行う)
    if let Some(g) = swap_guard.take() {
        g.commit()?;
    }
    // marker_dir_guard (TempDir) の drop で自動削除されるので、旧 remove_dir_all
    // の明示呼び出しは不要。marker_dir の PathBuf は値を保持するだけの
    // 副作用なし変数。
    drop(marker_dir_guard.take());
    let _ = marker_dir;

    if json {
        // `--top` は plain / JSON 両方に適用したいので、JSON 側でも truncate して
        // 出力する。元の report は mutate せず clone を加工する。
        let mut report = report.clone();
        if let Some(n) = top {
            report.plugins.truncate(n);
        }
        let v = crate::profile::report_to_json(&report);
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    if no_tui {
        crate::profile_tui::print_plain(&report, top);
        return Ok(());
    }

    crate::profile_tui::run(report)?;
    Ok(())
}

/// 起動時に前回 crash 由来の `loader.lua.bak` があれば検出して復元する。
///
/// ただし現在の loader.lua が `rvpm-profile-markers-` を含まない (= 既にクリーン
/// な generate で上書き済み) ケースでは、戻すべきでない古い bak を捨てる。
/// これをしないと「crash 後にユーザが `rvpm generate` で loader を更新 → 次の
/// profile 起動で古い bak が上書きしてロールバックしてしまう」事故が起きる。
fn recover_stale_loader_backup(loader_path: &Path) {
    let backup_path = loader_path.with_extension("lua.bak");
    if !backup_path.exists() {
        return;
    }
    // 現状の loader.lua が instrumented なら bak は生きた退避、そうでなければ
    // generate/sync で再生成済み → bak は不要。
    let current_is_instrumented = std::fs::read_to_string(loader_path)
        .map(|s| s.contains("rvpm-profile-markers-"))
        .unwrap_or(false);
    if !current_is_instrumented {
        eprintln!(
            "\u{26a0} rvpm: removing stale loader.lua.bak (current loader.lua is already clean)"
        );
        let _ = std::fs::remove_file(&backup_path);
        return;
    }
    eprintln!(
        "\u{26a0} rvpm: detected stale loader.lua.bak from a previous crashed profile run — restoring",
    );
    if loader_path.exists() {
        let _ = std::fs::remove_file(loader_path);
    }
    if let Err(e) = std::fs::rename(&backup_path, loader_path) {
        eprintln!(
            "\u{26a0} rvpm: could not auto-restore ({}). Run `rvpm generate` to rebuild.",
            e
        );
    }
}
