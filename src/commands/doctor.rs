use super::*;

/// `rvpm doctor` エントリポイント。config を読み、各チェックを走らせて
/// 診断レポートを stdout に出し、exit code を返す。
pub(crate) async fn run_doctor() -> Result<i32> {
    let config_path = rvpm_config_path();
    // config 読み込み / parse の失敗は通常チェックに入れず、専用の Config カテゴリ
    // で 1 件だけ報告する。icons は config が無い (= まだ読めていない) ので
    // デフォルトスタイルで描画する。
    let fallback_icons = crate::tui::Icons::from_style(crate::config::IconStyle::default());
    let toml_content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            // io エラーの種類で hint を出し分ける。NotFound 以外 (権限など) で
            // 「create the file」を案内すると誤誘導になるため。
            let hint = match e.kind() {
                std::io::ErrorKind::NotFound => "run `rvpm init --write` or create the file",
                std::io::ErrorKind::PermissionDenied => {
                    "check the file permissions on the config path"
                }
                _ => "check the config path and that the file is readable",
            };
            let diag = crate::doctor::Diagnostic::config_error(
                format!("failed to read {}: {}", config_path.display(), e),
                Some(hint),
            );
            print!("{}", crate::doctor::render(&[diag], &fallback_icons));
            return Ok(1);
        }
    };
    let mut config = match parse_config(&toml_content) {
        Ok(c) => c,
        Err(e) => {
            // parse_config は TOML 構文エラーだけでなく Tera 展開や型検証の失敗も
            // 返すので「syntax error」と決めつけない。原因は message に含める。
            let diag = crate::doctor::Diagnostic::config_error(
                format!("failed to load {}: {}", config_path.display(), e),
                Some("fix the reported error in config.toml and rerun `rvpm doctor`"),
            );
            print!("{}", crate::doctor::render(&[diag], &fallback_icons));
            return Ok(1);
        }
    };
    // sort_plugins は副作用で stderr に出るがエラーにはならない。doctor は
    // 自前で cycles / missing refs を検出するので sort_plugins は呼ばない。
    for plugin in config.plugins.iter_mut() {
        disable_merge_if_cond(plugin);
    }

    let cache_root = resolve_cache_root(config.options.cache_root.as_deref());
    let merged_dir = resolve_merged_dir(&cache_root);
    let loader_path = resolve_loader_path(&cache_root);
    let init_lua_path = nvim_init_lua_path();
    let repos_dir = resolve_repos_dir(&cache_root);

    // 未使用 repo の検出 (find_unused_repos を再利用)。repos_dir が無い場合は空。
    let mut unused: Vec<PathBuf> = if repos_dir.exists() {
        find_unused_repos(&config, &cache_root, &repos_dir).unwrap_or_default()
    } else {
        Vec::new()
    };
    unused.sort();

    // appname coherence
    let rvpm_env = std::env::var("RVPM_APPNAME").ok();
    let nvim_env = std::env::var("NVIM_APPNAME").ok();
    let resolved = appname();

    // resolve_dst は doctor 内で clone() 可能なクロージャに閉じ込める
    let cache_root_for_fn = cache_root.clone();
    let resolve_dst = Box::new(move |p: &crate::config::Plugin| -> PathBuf {
        resolve_plugin_dst(p, &cache_root_for_fn)
    });

    // helptags チェック用の target list を、本物の loader 生成と同じ規則で構築する。
    // merged + lazy + non-merge eager だけが個別の `:helptags` 対象になる。
    // lazy → eager 昇格も考慮するため、build_plugin_scripts → promote まで通す。
    let config_root_for_scripts = resolve_config_root(config.options.config_root.as_deref());
    let views_dir_for_scripts = resolve_views_dir(&cache_root);
    let mut plugin_scripts: Vec<crate::loader::PluginScripts> = Vec::new();
    for plugin in &config.plugins {
        let dst = resolve_plugin_dst(plugin, &cache_root);
        let plugin_config_dir = resolve_plugin_config_dir(&config_root_for_scripts, plugin);
        let view_dir = resolve_plugin_view_dir(&views_dir_for_scripts, plugin);
        let mode = decide_merge_mode(
            plugin.merge,
            plugin.lazy,
            plugin.merge_doc,
            config.options.merge_doc,
        );
        plugin_scripts.push(build_plugin_scripts(
            plugin,
            &dst,
            &plugin_config_dir,
            &view_dir,
            mode,
        ));
    }
    crate::loader::promote_lazy_to_eager(&mut plugin_scripts);
    let helptag_targets = crate::helptags::collect_helptag_targets(&plugin_scripts, &merged_dir);
    // collect_helptag_targets と同じイテレーションでラベルを並べる (順序を揃える)。
    // ラベルの判定根拠も collect_helptag_targets と一致させる: clone path 直下の
    // `doc/` ではなく **`view_path` 配下の `doc/`** で判定する (#119, CodeRabbit
    // PR #120)。 ViewWithoutDoc plugin は view に doc が無いので自動的に skip
    // され、 target list と label list の長さも一致する。
    let mut helptag_target_labels: Vec<String> = Vec::with_capacity(helptag_targets.len());
    if merged_dir.join("doc").is_dir() {
        helptag_target_labels.push("merged".to_string());
    }
    for ps in &plugin_scripts {
        if ps.merge && !ps.lazy {
            continue;
        }
        if PathBuf::from(&ps.view_path).join("doc").is_dir() {
            helptag_target_labels.push(ps.name.clone());
        }
    }
    debug_assert_eq!(helptag_targets.len(), helptag_target_labels.len());

    let merge_conflicts_path = resolve_merge_conflicts_path(&cache_root);
    let ctx = crate::doctor::CheckContext {
        config: &config,
        config_path: &config_path,
        loader_path: &loader_path,
        init_lua_path: &init_lua_path,
        merged_dir: &merged_dir,
        merge_conflicts_path: &merge_conflicts_path,
        unused_cache_dirs: unused,
        appname_resolved: resolved,
        rvpm_appname_env: rvpm_env,
        nvim_appname_env: nvim_env,
        resolver: Box::new(crate::doctor::SystemResolver),
        resolve_dst,
        helptag_targets,
        helptag_target_labels,
    };

    let diagnostics = crate::doctor::run_checks(&ctx).await;
    let icons = crate::tui::Icons::from_style(config.options.icons);
    let output = crate::doctor::render(&diagnostics, &icons);
    print!("{}", output);

    let summary = crate::doctor::Summary::from(&diagnostics);
    Ok(summary.exit_code())
}
