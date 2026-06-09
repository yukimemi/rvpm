use super::*;

/// `rvpm clean` — git 操作なしで、config.toml に無いプラグインディレクトリだけを削除する。
/// プラグイン数が多い環境で `sync --prune` が重いケースの受け皿。
/// 非同期処理は無いので `async` は付けない (clippy::unused_async 回避)。
pub(crate) fn run_clean() -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
    let mut config = parse_config(&toml_content)?;
    // sync / generate と同じ正規化パイプラインを通す: cond プリパスで
    // `merge` / `merge_doc` の相互整合を取り、 後段で promote も適用する
    // (CodeRabbit PR #120 指摘 — 正規化を飛ばすと cond plugin の生きた view を
    // 誤削除したり、 promoted plugin の stale view が残ったりする)。
    crate::config::sort_plugins(&mut config.plugins)?;
    for plugin in config.plugins.iter_mut() {
        disable_merge_if_cond(plugin);
    }

    let cache_root = resolve_cache_root(config.options.cache_root.as_deref());
    let repos_dir = resolve_repos_dir(&cache_root);
    if !repos_dir.exists() {
        println!(
            "No repos directory at {} — nothing to clean.",
            repos_dir.display()
        );
        return Ok(());
    }

    // force=true で即削除。空なら helper は (0, []) を返すので別メッセージを出す。
    let (count, _leftover) = maybe_prune_unused_repos(&config, &cache_root, true);
    if count == 0 {
        println!(
            "No unused plugin directories under {}.",
            repos_dir.display()
        );
    }

    // views/ も同じ要領で sweep (#119)。 sync / generate と整合する expected を
    // 計算するため、 PluginScripts に変換してから promote_lazy_to_eager を通す。
    // 昇格された (元 lazy → eager + Full) plugin は view を持たないので expected から
    // 自動的に除外される。
    let views_dir = resolve_views_dir(&cache_root);
    if views_dir.exists() {
        let config_root = resolve_config_root(config.options.config_root.as_deref());
        let mut plugin_scripts: Vec<crate::loader::PluginScripts> = config
            .plugins
            .iter()
            .map(|plugin| {
                let dst = resolve_plugin_dst(plugin, &cache_root);
                let plugin_config_dir = resolve_plugin_config_dir(&config_root, plugin);
                let view_dir = resolve_plugin_view_dir(&views_dir, plugin);
                let mode = decide_merge_mode(
                    plugin.merge,
                    plugin.lazy,
                    plugin.merge_doc,
                    config.options.merge_doc,
                );
                build_plugin_scripts(plugin, &dst, &plugin_config_dir, &view_dir, mode)
            })
            .collect();
        crate::loader::promote_lazy_to_eager(&mut plugin_scripts);

        // 正規化後の plugin_scripts から expected を組み立てる。 promote 後は
        // ps.merge=true && ps.lazy=false (= Full) になるので、 ここでの再判定で
        // ViewWith* には倒れない (= 自然に view から外れる)。 generate と一致。
        let expected: std::collections::HashSet<PathBuf> = plugin_scripts
            .iter()
            .filter_map(|ps| {
                let mode =
                    decide_merge_mode(ps.merge, ps.lazy, ps.merge_doc, config.options.merge_doc);
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
        prune_stale_views(&views_dir, &expected);
    }

    Ok(())
}
