use super::*;

/// `rvpm log [query] [--last N] [--full] [--diff]` 本体。
///
/// 永続化 JSON を読み、`--diff` が指定されていれば対象 doc files の patch を
/// `git diff <from>..<to> -- <file>` で 1 ファイルずつ取得し、整形して stdout に出す。
pub(crate) async fn run_log(
    query: Option<String>,
    last: usize,
    full: bool,
    diff: bool,
) -> Result<()> {
    // config.toml は **1 回だけ** 読む。resilience 原則: 壊れていても log は見える
    // べきなので `Option<Config>` にして以降は参照使い回し。
    let config_path = rvpm_config_path();
    let config: Option<crate::config::Config> = if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(toml_content) => parse_config(&toml_content).ok(),
            Err(_) => None,
        }
    } else {
        None
    };

    let cache_root = config
        .as_ref()
        .map(|c| resolve_cache_root(c.options.cache_root.as_deref()))
        .unwrap_or_else(|| resolve_cache_root(None));
    let icons = config.as_ref().map(|c| c.options.icons).unwrap_or_default();
    let log_path = resolve_update_log_path(&cache_root);

    let log = crate::update_log::load_log(&log_path);
    // 上限を超える `--last` は MAX_RUNS に丸める。
    let last = last.clamp(1, crate::update_log::MAX_RUNS);
    // query の lowercase も 1 回だけ。
    let query_lower: Option<String> = query.as_deref().map(|q| q.to_lowercase());
    let matches_query = |name: &str| -> bool {
        match &query_lower {
            Some(q) => name.to_lowercase().contains(q.as_str()),
            None => true,
        }
    };

    // `--diff` 用の patch 取得は表示順 (新しい run から最大 `last` 件) にだけ実施し、
    // クエリでフィルタされたプラグインに限る (無駄な git diff を避ける)。
    // key は (url, from, to, file) で run を区別する。`--last 2 --diff` 時に同じ
    // plugin の同じ doc file が複数 run で変わっていても patch が上書きされない。
    let mut diffs: std::collections::HashMap<crate::update_log::DiffKey, String> =
        std::collections::HashMap::new();
    if diff {
        let mut shown = 0;
        for run in log.runs.iter().rev() {
            if shown >= last {
                break;
            }
            if !run.changes.iter().any(|c| matches_query(&c.name)) {
                continue;
            }
            shown += 1;
            for change in &run.changes {
                if !matches_query(&change.name) {
                    continue;
                }
                // 新規 clone (from = None) は from..to を作れないので skip
                let Some(from) = change.from.as_deref() else {
                    continue;
                };
                if change.doc_files_changed.is_empty() {
                    continue;
                }
                // dst_path は事前パース済み config から解決。config が無ければ skip。
                let Some(cfg) = config.as_ref() else { continue };
                let Some(plugin) = cfg.plugins.iter().find(|p| p.url == change.url) else {
                    continue;
                };
                let dst_path = resolve_plugin_dst(plugin, &cache_root);
                // 1 plugin 分の patch をまとめて取得 (repo open / tree diff は 1 回)。
                let patches = crate::git::doc_file_patches(
                    &dst_path,
                    from,
                    &change.to,
                    &change.doc_files_changed,
                );
                for file in &change.doc_files_changed {
                    if let Some(patch) = patches.get(file) {
                        diffs.insert(
                            crate::update_log::DiffKey {
                                url: change.url.clone(),
                                from: from.to_string(),
                                to: change.to.clone(),
                                file: file.clone(),
                            },
                            patch.clone(),
                        );
                    }
                }
            }
        }
    }

    let opts = crate::update_log::LogRenderOptions {
        last,
        query: query.as_deref(),
        full,
        diff,
        diffs,
        icons,
        now: std::time::SystemTime::now(),
    };
    let rendered = crate::update_log::render_log(&log, &opts);
    print!("{}", rendered);
    Ok(())
}
