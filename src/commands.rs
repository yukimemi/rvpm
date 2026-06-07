//! CLI subcommand handlers (#217 stage 3).
//!
//! Every `run_*` entry point invoked by the dispatcher in `lib.rs::run_cli`
//! lives here, moved verbatim out of the monolith. Shared helpers and the clap
//! `Cli` / `Commands` definitions stay in `lib.rs` and are reached via
//! `crate::` (glob-imported below). A later pass will split this file into
//! per-subcommand modules under `commands/` (#228).

use crate::config::parse_config;
use crate::git::Repo;
use crate::merge::*;
use crate::paths::*;
use crate::plugin_build::*;
use crate::tui::{PluginStatus, TuiState};
use crate::url::*;
use crate::*;
use anyhow::{Context, Result};
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dialoguer::{FuzzySelect, Select};
use ratatui::backend::CrosstermBackend;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use toml_edit::{DocumentMut, Item, table, value};

/// `rvpm completion <SHELL>` — clap_complete に CLI 定義を渡して
/// stdout に補完スクリプトを書き出す (#114)。
///
/// 補完スクリプトの内容は CLI 定義 (Cli / Commands enum) から自動生成されるので、
/// サブコマンドや flag を追加した時点で自動的に反映される。 `rvpm.nvim` 側の
/// `lua/rvpm/command.lua` (Neovim 用 :Rvpm 補完) は別管理なので、 そちらは
/// CLAUDE.md のチェックリスト通り手動で sync する必要がある。
pub(crate) fn run_completion(shell: clap_complete::Shell) {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    let bin = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin, &mut std::io::stdout());
}

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
                    let held_back = if fetched {
                        let remote_head = if plugin.rev.is_none() && effective_rev.is_some() {
                            repo.remote_head().await.ok().flatten()
                        } else {
                            None
                        };
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
                    let _ = tx.send((plugin.url.clone(), PluginStatus::Finished)).await;
                    Ok((plugin, dst_path, build_warn, change, head_commit, held_back, fetched))
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
                if let Ok(Ok((plugin, dst_path, build_warn, git_change, head_commit, pin, fetched))) = res {
                    if let Some(warn) = build_warn {
                        build_warnings.push((plugin.url.clone(), warn));
                    }
                    if let Some(change) = git_change {
                        sync_changes.push(change_record_from(&plugin, change));
                    }
                    if let Some(p) = pin {
                        held_back.push(p);
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

    print_init_lua_hint_if_missing(&config);
    Ok(())
}

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
            if let Some(s) = &merged_stamp {
                if let Err(e) = crate::view_stamp::write(tmp_merged, s) {
                    // stamp が書けなくても merged 自体は有効 — 次回 rebuild に倒れるだけ。
                    eprintln!("\u{26a0} failed to write merged stamp: {}", e);
                }
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
        config::Config,
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_add(
    repo: String,
    name: Option<String>,
    lazy: Option<bool>,
    on_cmd: Option<String>,
    on_ft: Option<String>,
    on_map: Option<String>,
    on_event: Option<String>,
    rev: Option<String>,
    policy_override: Option<crate::config::AutoLazyPolicy>,
    ai_override: Option<crate::config::AiBackend>,
) -> Result<()> {
    let config_path = rvpm_config_path();
    ensure_config_exists(&config_path)?;
    let toml_content = std::fs::read_to_string(&config_path)?;
    let mut doc = toml_content.parse::<DocumentMut>()?;
    // url_style は DocumentMut から直接読む (parse_config 経由だと Tera 展開と
    // 全フィールドのデシリアライズが走って無駄。ここでは add に必要な
    // option だけ拾えればよい)。値が無効 / 読めないなら default (Short)。
    let url_style = doc
        .get("options")
        .and_then(|o| o.get("url_style"))
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "short" => Some(crate::config::UrlStyle::Short),
            "full" => Some(crate::config::UrlStyle::Full),
            _ => None,
        })
        .unwrap_or_default();
    if doc.get("plugins").is_none() {
        doc["plugins"] = toml_edit::ArrayOfTables::new().into();
    }
    let plugins = doc["plugins"]
        .as_array_of_tables_mut()
        .context("plugins is not an array of tables")?;
    // 重複検出: browse TUI と同じ `installed_full_name` で両辺を正規化して比較。
    // https / short / ssh / 大文字小文字 / `.git` / 末尾 `/` の揺れを吸収。
    // どちらかが GitHub URL と認識できない (gitlab 等) なら生文字列一致に fallback。
    let incoming_normalized = installed_full_name(&repo);
    for p in plugins.iter() {
        let existing_url = p.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let existing_normalized = installed_full_name(existing_url);
        let matches = match (&incoming_normalized, &existing_normalized) {
            (Some(a), Some(b)) => a == b,
            _ => existing_url == repo,
        };
        if matches {
            println!("Plugin already exists: {}", existing_url);
            return Ok(());
        }
    }
    // 書き込み URL は options.url_style に従って整形 (GitHub 以外はそのまま)。
    let stored_url = format_plugin_url(&repo, url_style);

    // user が明示的に CLI flag で指定したキー一覧 — AI mode 適用時はこれを尊重して
    // 上書きしない (#95 review CodeRabbit Major)。`url` は常に保護 (canonical 化される)。
    let preserved_keys: Vec<&'static str> = {
        let mut k = vec!["url"];
        if name.is_some() {
            k.push("name");
        }
        if lazy.is_some() {
            k.push("lazy");
        }
        if rev.is_some() {
            k.push("rev");
        }
        if on_cmd.is_some() {
            k.push("on_cmd");
        }
        if on_ft.is_some() {
            k.push("on_ft");
        }
        if on_map.is_some() {
            k.push("on_map");
        }
        if on_event.is_some() {
            k.push("on_event");
        }
        k
    };

    let mut new_plugin = table();
    new_plugin["url"] = value(&stored_url);
    if let Some(n) = name {
        new_plugin["name"] = value(n);
    }
    if let Some(l) = lazy {
        new_plugin["lazy"] = value(l);
    }
    if let Some(r) = &rev {
        new_plugin["rev"] = value(r.as_str());
    }
    if let Item::Table(t) = new_plugin {
        plugins.push(t);
    }
    // on_* フラグがあれば set_plugin_list_field / set_plugin_map_field で追加。
    // 検索キーは上で書き込んだ `stored_url` に揃える必要がある — `repo` のままだと
    // `options.url_style = "full"` で `owner/repo` → `https://github.com/owner/repo`
    // に書き換えたとき entry 名とキーが一致せず、no-op になる。
    let maybe_parse = |raw: Option<String>| -> Result<Option<Vec<String>>> {
        raw.map(|s| parse_cli_string_list(&s)).transpose()
    };
    if let Some(items) = maybe_parse(on_cmd)? {
        set_plugin_list_field(&mut doc, &stored_url, "on_cmd", items)?;
    }
    if let Some(items) = maybe_parse(on_ft)? {
        set_plugin_list_field(&mut doc, &stored_url, "on_ft", items)?;
    }
    if let Some(raw) = on_map {
        let specs = parse_on_map_cli(&raw)?;
        set_plugin_map_field(&mut doc, &stored_url, specs)?;
    }
    if let Some(items) = maybe_parse(on_event)? {
        set_plugin_list_field(&mut doc, &stored_url, "on_event", items)?;
    }

    let toml_content = doc.to_string();
    let chezmoi_enabled = read_chezmoi_flag(&config_path);
    chezmoi::write_routed(chezmoi_enabled, &config_path, &toml_content).await?;
    println!("Added plugin to config: {}", stored_url);

    // 追加したプラグインだけ clone + merge し、loader.lua を再生成する
    let config_data = parse_config(&toml_content)?;
    let cache_root = resolve_cache_root(config_data.options.cache_root.as_deref());
    let merged_dir = resolve_merged_dir(&cache_root);

    // `stored_url` は format_plugin_url で canonical 化された URL なので、
    // config.toml に書き込んだ entry とそのまま一致する。`repo` (入力のまま) で
    // 引くと url_style="full" のときミスマッチで clone が走らなくなる。
    let mut add_changes: Vec<crate::update_log::ChangeRecord> = Vec::new();
    // 新規追加したプラグインの HEAD を lockfile にも記録する (dotfiles
    // にコミットすれば他マシンでも同じ commit を再現できる)。
    let config_root_for_lock = resolve_config_root(config_data.options.config_root.as_deref());
    let lockfile_path = resolve_lockfile_path(&config_root_for_lock);
    let mut lockfile = crate::lockfile::LockFile::load(&lockfile_path);
    let mut lockfile_dirty = false;

    // **fetch_state も load しておく**: clone 完了後に `last_fetched` を upsert
    // するため。これをやらないと、`rvpm add` 直後に `rvpm sync` を回したとき、
    // `fetch_state.find(name) → None` で should_fetch が "fetch する" を返し、
    // 直前 clone した repo に対して即 fetch を発射する。Docker overlay /
    // WSL2 のような race の起きやすい FS で gix の "Failed to update
    // references to their new position to match their remote locations"
    // を踏みやすい (user 報告)。run_sync が done している upsert と等価な
    // 後始末を run_add も負担すべき。
    let fetch_state_path = resolve_fetch_state_path(&cache_root);
    let mut fetch_state = crate::fetch_state::FetchState::load(&fetch_state_path);
    let mut fetch_state_dirty = false;
    if let Some(mut plugin) = config_data
        .plugins
        .iter()
        .find(|p| p.url == stored_url)
        .cloned()
    {
        disable_merge_if_cond(&mut plugin);
        let dst_path = resolve_plugin_dst(&plugin, &cache_root);

        println!("Syncing {}...", plugin.display_name());
        let git_repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
        match git_repo.sync().await {
            Err(e) => {
                eprintln!("Warning: failed to sync '{}': {}", plugin.display_name(), e);
            }
            Ok(change) => {
                if let Some(c) = change {
                    add_changes.push(change_record_from(&plugin, c));
                }
                if let Some(err) =
                    execute_build_command(&plugin, &dst_path, &config_data, &cache_root).await
                {
                    eprintln!("Warning: {}: {}", plugin.display_name(), err);
                }
                // lockfile 記録 (no-op sync でも head_commit は読める)
                if !plugin.dev
                    && let Ok(commit) = git_repo.head_commit().await
                {
                    lockfile.upsert(crate::lockfile::LockEntry {
                        name: plugin.display_name(),
                        url: plugin.url.clone(),
                        commit,
                    });
                    lockfile_dirty = true;
                }

                // fetch_state はこのブロックの末尾で upsert する (CodeRabbit PR
                // #107 指摘)。AI / auto-lazy の patch path が config.toml の
                // `name` を変更する可能性があるため、`plugin` のスナップショット
                // ではなく **AI 適用後の最終 name** で記録する必要がある。
                // 実際の upsert は Ok-arm の最後で `read_persisted_plugin_name`
                // を使って行う。

                // run_add 直後に merge する必要は無い: 末尾で `run_generate()` を呼び、
                // そこで merged/ を rm -rf して全 eager+merge を再構築するため。
                // (旧実装はここで merge していたが run_generate に上書きされて冗長)
                let _ = (&plugin, &dst_path, &merged_dir);

                // ── ここで mode 分岐 ────────────────────────────
                // AI 設定が `Off` 以外なら静的 scan を skip して AI 経路 (#93)。
                // それ以外は従来の auto-lazy scan + prompt (#87) に流す。
                let effective_ai = ai_override.unwrap_or(config_data.options.ai);
                if let Ok(backend) = crate::ai::Backend::try_from(effective_ai) {
                    let cfg_root = resolve_config_root(config_data.options.config_root.as_deref());
                    let plugin_cfg_dir = resolve_plugin_config_dir(&cfg_root, &plugin);
                    match crate::ai::run_ai_add(
                        backend,
                        &stored_url,
                        &dst_path,
                        &plugin_cfg_dir,
                        &cfg_root,
                        &config_path,
                        &config_data.options.ai_language,
                        config_data.options.chezmoi,
                    )
                    .await
                    {
                        Ok(outcome) => match outcome.outcome {
                            crate::ai::ChatOutcome::Applied { hook_changes } => {
                                // user が `[[plugins]]` セクションで "Keep existing" を選んだら
                                // `plugin_entry_toml` は None — stub entry をそのまま残す。
                                if let Some(entry_toml) = outcome.plugin_entry_toml {
                                    let latest = std::fs::read_to_string(&config_path)?;
                                    let mut doc_patch = latest.parse::<DocumentMut>()?;
                                    if let Err(e) = replace_plugin_entry_with_ai_toml(
                                        &mut doc_patch,
                                        &stored_url,
                                        &entry_toml,
                                        &preserved_keys,
                                        MergeMode::Merge,
                                    ) {
                                        eprintln!(
                                            "\u{26a0} failed to apply AI proposal: {e}. Stub entry remains."
                                        );
                                    } else {
                                        let patched = doc_patch.to_string();
                                        chezmoi::write_routed(
                                            config_data.options.chezmoi,
                                            &config_path,
                                            &patched,
                                        )
                                        .await?;
                                        println!(
                                            "Applied AI-proposed config for {} ({} hook(s) written, {} removed).",
                                            plugin.display_name(),
                                            hook_changes.written.len(),
                                            hook_changes.removed.len()
                                        );
                                    }
                                } else {
                                    // `add --ai` には "keep existing" の概念が無い (stub
                                    // entry しか存在しない) ので、ここに来るのは chat /
                                    // apply の regression を示す。silent に stub を残すと
                                    // 「green path に化けたバグ」になるので明示 fail する
                                    // (CodeRabbit PR #104 post-merge レビュー指摘)。
                                    //
                                    // **state note (Gemini PR #105 指摘)**: stub entry は
                                    // run_add の冒頭で既に config.toml に書き込まれている。
                                    // ここでの bail はそれを rollback しないので、stub は
                                    // disk に残ったまま。message でその事実 + リカバリ手段を
                                    // 明示し、「refusing to keep」みたいな誤読しやすい表現は
                                    // 避ける。
                                    anyhow::bail!(
                                        "AI add returned no [[plugins]] proposal for {0}. \
                                         The stub entry written to {1} is left as-is — \
                                         remove it manually with `rvpm remove {0}` or rerun \
                                         `rvpm add` with `--no-ai` for the static-scan path.",
                                        plugin.display_name(),
                                        config_path.display()
                                    );
                                }
                            }
                            crate::ai::ChatOutcome::Skipped => {
                                eprintln!(
                                    "AI proposal skipped — stub entry kept in config.toml. \
                                     Edit manually or rerun `rvpm add {repo} --no-ai` for static-scan mode."
                                );
                            }
                            crate::ai::ChatOutcome::HandedOff => {
                                eprintln!(
                                    "Handed off to {} CLI. rvpm exits — that session controls config.toml from here.",
                                    backend.label()
                                );
                            }
                        },
                        Err(e) => {
                            eprintln!(
                                "\u{26a0} AI add failed: {e:#}. Stub entry kept; rerun with `--no-ai` for static-scan mode."
                            );
                            eprintln!(
                                "\n  Debug knobs (env vars):\n\
                                 \x20 RVPM_AI_DUMP_PROMPT=/tmp/p.md   write the prompt to a file and skip the AI call\n\
                                 \x20 RVPM_AI_NO_MERGED=1             drop the `_merged` variant requirement (helps if\n\
                                 \x20                                 the backend's loop-detection trips on near-duplicate\n\
                                 \x20                                 fresh+merged output, e.g. gemini's _recoverFromLoop)\n\
                                 \x20 RVPM_AI_TIMEOUT_SECS=600        raise the per-call timeout (default 300)"
                            );
                        }
                    }
                } else {
                    // ── auto-lazy scan + prompt (#87) — 従来路 ──────────
                    let policy = resolve_add_lazy_policy(policy_override, &config_data);
                    let skip_for_explicit_eager = plugin.lazy_raw == Some(false);
                    if !skip_for_explicit_eager
                        && !matches!(policy, crate::config::AutoLazyPolicy::Never)
                        && let Some(suggestion) =
                            build_add_suggestion(&crate::plugin_scan::scan_plugin(&dst_path))
                        && let Some(applied) =
                            decide_add_lazy_apply(suggestion, policy, &plugin.display_name())
                    {
                        let latest = std::fs::read_to_string(&config_path)?;
                        let mut doc_patch = latest.parse::<DocumentMut>()?;
                        patch_plugin_entry_triggers(&mut doc_patch, &stored_url, &applied);
                        let patched = doc_patch.to_string();
                        let wp =
                            chezmoi::write_path(config_data.options.chezmoi, &config_path).await;
                        std::fs::write(&wp, &patched)?;
                        chezmoi::apply(&wp, &config_path).await;
                        println!(
                            "Recorded lazy triggers for {} in config.toml.",
                            plugin.display_name()
                        );
                    }
                }

                // fetch_state 記録 (deferred): clone / pull が成功したので
                // 「今 fetch した」とマーク。これで直後の `rvpm sync` が
                // fetch_interval fast-path を通って同じ repo に再 fetch を
                // かけずに済み、Docker overlay / WSL2 で gix が踏みやすい
                // post-fetch ref-update race を避けられる (user 報告)。
                //
                //   - `if !plugin.dev` ガード: lockfile + run_sync と挙動を統一
                //     (Gemini PR #107 指摘)。dev plugin は fetch_state 管理対象外。
                //   - **AI / auto-lazy patch 後の `name`** を読みに行く: 上の
                //     branch で config.toml の `name` フィールドが書き換わって
                //     いる可能性があるため、`plugin.display_name()` のスナップ
                //     ショットではなく `read_persisted_plugin_name` で再取得
                //     (CodeRabbit PR #107 指摘)。
                if !plugin.dev {
                    let final_name =
                        read_persisted_plugin_name(&config_path, &stored_url, &plugin.url);
                    fetch_state.upsert(crate::fetch_state::FetchEntry {
                        name: final_name,
                        url: plugin.url.clone(),
                        last_fetched: crate::fetch_state::now_rfc3339(),
                    });
                    fetch_state_dirty = true;
                }
            }
        }
    }

    record_changes_or_warn(&cache_root, "add", add_changes);

    if lockfile_dirty {
        // chezmoi 連携: source 側に書いてから `chezmoi apply` で target に反映。
        let wp = chezmoi::write_path(config_data.options.chezmoi, &lockfile_path).await;
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

    // fetch_state.json は cache_root 配下の local-only データなので chezmoi
    // 経路は通さない (run_sync の終端と同じ扱い)。failure は warn のみ —
    // resilience: 本処理は終わってるので fetch_state save 失敗で全体を倒さない。
    if fetch_state_dirty && let Err(e) = fetch_state.save(&fetch_state_path) {
        eprintln!(
            "\u{26a0} failed to save {}: {} (fetch_state not updated)",
            fetch_state_path.display(),
            e
        );
    }

    run_generate(false).await?;
    Ok(())
}

/// `rvpm config` — config.toml を $EDITOR で直接開く。
/// ファイルが無ければテンプレートで自動作成してから開く。
/// 編集前後の mtime を比較して、 **実際に変更があった場合のみ `Ok(true)` を返す**。
/// 呼び出し側 (rvpm list TUI の `c` キー等) は戻り値で sync / generate を条件実行する。
pub(crate) async fn run_config() -> Result<bool> {
    let config_path = rvpm_config_path();
    ensure_config_exists(&config_path)?;
    let chezmoi_enabled = read_chezmoi_flag(&config_path);
    let edit_target = chezmoi::write_path(chezmoi_enabled, &config_path).await;
    println!("Opening {}", edit_target.display());
    // mtime を編集前後で比較して、 config.toml に変更が無ければ caller (rvpm list
    // TUI 等) が後続の `run_generate` を skip できるようにする。 todoke 系の
    // 「open & close せず別 nvim instance に send」ワークフローで、 編集してない
    // のに毎回 generate が走ると view tree の rebuild race を引き起こす #119。
    let before_mtime = std::fs::metadata(&edit_target)
        .and_then(|m| m.modified())
        .ok();
    open_editor_at_line(&edit_target, 1)?;
    let after_mtime = std::fs::metadata(&edit_target)
        .and_then(|m| m.modified())
        .ok();
    chezmoi::apply(&edit_target, &config_path).await;
    Ok(before_mtime != after_mtime)
}

/// `rvpm init` — Neovim init.lua に loader.lua を繋ぐ dofile 行を案内 or 自動追記する。
pub(crate) async fn run_init(write: bool) -> Result<()> {
    // config.toml がなければテンプレートで自動作成 (add / config と同じ)
    let config_path = rvpm_config_path();
    ensure_config_exists(&config_path)?;
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
    let config = parse_config(&toml_content)?;

    let snippet = loader_init_snippet(&config);
    let init_lua_path = nvim_init_lua_path();

    if write {
        // `config` は既にパース済みなので再読込せずそのまま使う。
        // 親ディレクトリ作成は write_init_lua_snippet が新規作成時に行うので不要。
        let wp = chezmoi::write_path(config.options.chezmoi, &init_lua_path).await;
        let result = write_init_lua_snippet(&wp, &snippet)?;
        match result {
            WriteInitResult::Created => {
                println!("\u{2714} Created {} with rvpm loader.", wp.display());
                println!("  Snippet: {}", snippet);
            }
            WriteInitResult::Appended => {
                println!("\u{2714} Appended rvpm loader to {}.", wp.display());
                println!("  Snippet: {}", snippet);
            }
            WriteInitResult::AlreadyConfigured => {
                println!(
                    "\u{2714} {} already references rvpm loader. No changes.",
                    wp.display()
                );
            }
        }
        // 実際に source 側を書き換えたときだけ chezmoi apply する。変更なしの
        // AlreadyConfigured で apply すると、target 側でユーザーが手で編集した
        // 差分を上書きしてしまう恐れがある。
        if result != WriteInitResult::AlreadyConfigured {
            chezmoi::apply(&wp, &init_lua_path).await;
        }
    } else {
        println!("-- Add this to your Neovim init.lua:");
        println!("{}", snippet);
        println!();
        println!("Target: {}", init_lua_path.display());
        println!("Or run `rvpm init --write` to append it automatically.");
    }
    Ok(())
}

pub(crate) async fn run_edit(
    query: Option<String>,
    flag_init: bool,
    flag_before: bool,
    flag_after: bool,
    flag_global: bool,
) -> Result<bool> {
    // --global: グローバル hooks。init.lua は **Neovim 本体の init.lua**
    // (~/.config/<appname>/init.lua) を指す — `rvpm init` と同じ対象。before.lua /
    // after.lua は rvpm の <config_root> 配下。per-plugin の init/before/after と
    // 同じ 3 択になり、`rvpm edit --init --global` で Neovim 本体 init.lua を
    // 直接開ける。
    if flag_global {
        // config_root を決めるため config.toml を先読み (存在しなければデフォルト)。
        let config_path = rvpm_config_path();
        let config_root = if config_path.exists() {
            let toml_content = std::fs::read_to_string(&config_path)?;
            let config = parse_config(&toml_content)?;
            resolve_config_root(config.options.config_root.as_deref())
        } else {
            resolve_config_root(None)
        };
        let config_dir = config_root.clone();
        std::fs::create_dir_all(&config_dir)?;
        let nvim_init = nvim_init_lua_path();

        // (file_name, target_path) のペア。before/after は config_dir 配下、
        // init.lua のみ Neovim 本体の path に飛ばす。
        let target = if flag_init {
            nvim_init.clone()
        } else if flag_before {
            config_dir.join("before.lua")
        } else if flag_after {
            config_dir.join("after.lua")
        } else {
            let entries: [(&str, PathBuf); 3] = [
                ("init.lua", nvim_init.clone()),
                ("before.lua", config_dir.join("before.lua")),
                ("after.lua", config_dir.join("after.lua")),
            ];
            let display_items: Vec<String> = entries
                .iter()
                .map(|(label, path)| {
                    let icon = if path.exists() {
                        "\u{25cf}"
                    } else {
                        "\u{25cb}"
                    };
                    format!("{} {}", icon, label)
                })
                .collect();
            let sel = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                .with_prompt("Select global hook to edit (\u{25cf}=exists \u{25cb}=new)")
                .default(0)
                .items(&display_items)
                .interact_opt()?;
            match sel {
                Some(index) => entries[index].1.clone(),
                None => return Ok(false),
            }
        };

        let chezmoi_enabled = read_chezmoi_flag(&config_path);
        let edit_target = chezmoi::write_path(chezmoi_enabled, &target).await;
        if let Some(parent) = edit_target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        println!("\n>> Editing global hook: {}", edit_target.display());
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
        std::process::Command::new(editor)
            .arg(&edit_target)
            .status()?;
        chezmoi::apply(&edit_target, &target).await;
        return Ok(true);
    }

    // per-plugin edit
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    // 対話モード: plugin 選択肢に [ Global hooks ] sentinel を追加
    // 各プラグインの init/before/after.lua 存在をサークルアイコンで表示
    let config_root = resolve_config_root(config.options.config_root.as_deref());
    // global hook のアイコン表示用 (実使用は run_edit --global 経由)
    let config_dir = config_root.clone();

    let plugin = if let Some(q) = query {
        config
            .plugins
            .iter()
            .find(|p| p.url == q || p.url.contains(&q))
            .context("Plugin not found")?
    } else {
        // URL の最大幅を揃えてサークルを右に並べる
        let global_label = "[ Global hooks ]".to_string();
        let max_url_len = config
            .plugins
            .iter()
            .map(|p| p.url.len())
            .max()
            .unwrap_or(20)
            .max(global_label.len());

        let global_indicators = global_hook_indicators(&config_dir, &nvim_init_lua_path());
        let mut items: Vec<String> = vec![format!(
            "{:<width$}  {}",
            global_label,
            global_indicators,
            width = max_url_len
        )];
        let mut urls: Vec<String> = vec![String::new()]; // sentinel placeholder

        for p in config.plugins.iter() {
            let plugin_config_dir = resolve_plugin_config_dir(&config_root, p);
            let indicators = hook_indicators(&plugin_config_dir);
            let has_any = plugin_config_dir.join("init.lua").exists()
                || plugin_config_dir.join("before.lua").exists()
                || plugin_config_dir.join("after.lua").exists();
            let suffix = if has_any {
                format!("  {}", indicators)
            } else {
                String::new()
            };
            items.push(format!("{:<width$}{}", p.url, suffix, width = max_url_len));
            urls.push(p.url.clone());
        }

        let selection = FuzzySelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Select plugin to edit (I=init B=before A=after)")
            .default(0)
            .items(&items)
            .interact_opt()?;
        match selection {
            Some(0) => {
                return Box::pin(run_edit(None, false, false, false, true)).await;
            }
            Some(index) => config
                .plugins
                .iter()
                .find(|p| p.url == urls[index])
                .unwrap(),
            None => return Ok(false),
        }
    };

    println!("\n>> Editing configuration for: {}", plugin.url);

    let plugin_config_dir = resolve_plugin_config_dir(&config_root, plugin);

    // --init / --before / --after フラグがあれば対話式をスキップ
    let file_name = if flag_init {
        "init.lua"
    } else if flag_before {
        "before.lua"
    } else if flag_after {
        "after.lua"
    } else {
        let file_names = ["init.lua", "before.lua", "after.lua"];
        let display_items: Vec<String> = file_names
            .iter()
            .map(|f| file_with_icon(&plugin_config_dir, f))
            .collect();
        let file_selection = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Select file to edit (\u{25cf}=exists \u{25cb}=new)")
            .default(0)
            .items(&display_items)
            .interact_opt()?;
        match file_selection {
            Some(index) => file_names[index],
            None => return Ok(false),
        }
    };
    let target_file = plugin_config_dir.join(file_name);
    let chezmoi_enabled = read_chezmoi_flag(&config_path);
    let edit_target = chezmoi::write_path(chezmoi_enabled, &target_file).await;
    if let Some(parent) = edit_target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
    std::process::Command::new(editor)
        .arg(&edit_target)
        .status()?;
    chezmoi::apply(&edit_target, &target_file).await;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_set(
    query: Option<String>,
    lazy: Option<bool>,
    merge: Option<bool>,
    on_cmd: Option<String>,
    on_ft: Option<String>,
    on_map: Option<String>,
    on_event: Option<String>,
    on_path: Option<String>,
    on_source: Option<String>,
    rev: Option<String>,
) -> Result<bool> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    let Some(selected_repo_url) =
        select_plugin_url(&config.plugins, query.as_deref(), "Select plugin to set")?
    else {
        return Ok(false);
    };

    println!("\n>> Setting options for: {}", selected_repo_url);
    let mut doc = toml_content.parse::<DocumentMut>()?;
    let mut modified = false;

    let any_flag_set = lazy.is_some()
        || merge.is_some()
        || on_cmd.is_some()
        || on_ft.is_some()
        || on_map.is_some()
        || on_event.is_some()
        || on_path.is_some()
        || on_source.is_some()
        || rev.is_some();

    if any_flag_set {
        // Option<String> → Result<Option<Vec<String>>> へ (malformed JSON はエラー)
        let maybe_parse = |raw: Option<String>| -> Result<Option<Vec<String>>> {
            raw.map(|s| parse_cli_string_list(&s)).transpose()
        };

        update_plugin_config(
            &mut doc,
            &selected_repo_url,
            lazy,
            merge,
            maybe_parse(on_cmd)?,
            maybe_parse(on_ft)?,
            rev,
        )?;
        // on_map は table 形式 (mode/desc) をサポートするため専用パーサを通す
        if let Some(raw) = on_map {
            let specs = parse_on_map_cli(&raw)?;
            set_plugin_map_field(&mut doc, &selected_repo_url, specs)?;
        }
        if let Some(items) = maybe_parse(on_event)? {
            set_plugin_list_field(&mut doc, &selected_repo_url, "on_event", items)?;
        }
        if let Some(items) = maybe_parse(on_path)? {
            set_plugin_list_field(&mut doc, &selected_repo_url, "on_path", items)?;
        }
        if let Some(items) = maybe_parse(on_source)? {
            set_plugin_list_field(&mut doc, &selected_repo_url, "on_source", items)?;
        }
        modified = true;
    } else {
        // 現在のプラグインを探して既存値をプレフィルに使う
        let current_plugin = config
            .plugins
            .iter()
            .find(|p| p.url == selected_repo_url)
            .cloned();
        let list_field_value = |field: &str| -> String {
            let Some(p) = current_plugin.as_ref() else {
                return String::new();
            };
            // on_map は MapSpec の lhs だけを列挙する (mode/desc は手書き編集に委ねる)
            let items: Option<Vec<String>> = match field {
                "on_cmd" => p.on_cmd.clone(),
                "on_ft" => p.on_ft.clone(),
                "on_map" => p
                    .on_map
                    .as_ref()
                    .map(|v| v.iter().map(|m| m.lhs.clone()).collect()),
                "on_event" => p.on_event.clone(),
                "on_path" => p.on_path.clone(),
                "on_source" => p.on_source.clone(),
                _ => None,
            };
            items.map(|v| v.join(", ")).unwrap_or_default()
        };

        const EDITOR_SENTINEL: &str = "[ Open config.toml in $EDITOR ]";
        let options = vec![
            EDITOR_SENTINEL,
            "lazy",
            "merge",
            "on_cmd",
            "on_ft",
            "on_map",
            "on_event",
            "on_path",
            "on_source",
            "rev",
        ];
        let selection = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Select option to set")
            .default(0)
            .items(&options)
            .interact_opt()?;
        match selection {
            Some(index) => {
                match options[index] {
                    s if s == EDITOR_SENTINEL => {
                        // 対応 editor なら plugin の url 行にジャンプ
                        let line = find_plugin_line_in_toml(&toml_content, &selected_repo_url);
                        let cz = read_chezmoi_flag(&config_path);
                        let ep = chezmoi::write_path(cz, &config_path).await;
                        open_editor_at_line(&ep, line)?;
                        chezmoi::apply(&ep, &config_path).await;
                        // ユーザーが何を編集したか分からないので常に変更ありと見なす
                        return Ok(true);
                    }
                    "lazy" | "merge" => {
                        let current = current_plugin
                            .as_ref()
                            .map(|p| {
                                if options[index] == "lazy" {
                                    p.lazy
                                } else {
                                    p.merge
                                }
                            })
                            .unwrap_or(false);
                        let default_idx = if current { 0 } else { 1 };
                        let val = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                            .with_prompt(format!(
                                "Set {} to (current: {})",
                                options[index], current
                            ))
                            .items(["true", "false"])
                            .default(default_idx)
                            .interact_opt()?;
                        if let Some(v) = val {
                            update_plugin_config(
                                &mut doc,
                                &selected_repo_url,
                                if options[index] == "lazy" {
                                    Some(v == 0)
                                } else {
                                    None
                                },
                                if options[index] == "merge" {
                                    Some(v == 0)
                                } else {
                                    None
                                },
                                None,
                                None,
                                None,
                            )?;
                            modified = true;
                        } else {
                            return Ok(false);
                        }
                    }
                    "on_map" => {
                        // on_map は table 形式 (mode/desc) もあるので edit mode を先に聞く
                        let modes = &[
                            "Edit lhs list only (CLI, mode/desc lost)",
                            "Open config.toml in $EDITOR",
                        ];
                        let mode_sel =
                            Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                                .with_prompt("on_map edit mode")
                                .items(modes)
                                .default(0)
                                .interact_opt()?;
                        match mode_sel {
                            Some(0) => {
                                // CLI: lhs のみ編集 (既存の簡易フロー)
                                let existing = list_field_value("on_map");
                                let val = read_input_with_esc(
                                    "Enter on_map lhs values (comma separated, Esc to cancel)",
                                    &existing,
                                )?;
                                match val {
                                    Some(v) if !v.is_empty() => {
                                        let items: Vec<String> = v
                                            .split(',')
                                            .map(|s| s.trim().to_string())
                                            .filter(|s| !s.is_empty())
                                            .collect();
                                        set_plugin_list_field(
                                            &mut doc,
                                            &selected_repo_url,
                                            "on_map",
                                            items,
                                        )?;
                                        modified = true;
                                    }
                                    _ => return Ok(false),
                                }
                            }
                            Some(1) => {
                                let line =
                                    find_plugin_line_in_toml(&toml_content, &selected_repo_url);
                                let cz = read_chezmoi_flag(&config_path);
                                let ep = chezmoi::write_path(cz, &config_path).await;
                                open_editor_at_line(&ep, line)?;
                                chezmoi::apply(&ep, &config_path).await;
                                return Ok(true);
                            }
                            _ => return Ok(false),
                        }
                    }
                    field @ ("on_cmd" | "on_ft" | "on_event" | "on_path" | "on_source") => {
                        let existing = list_field_value(field);
                        let val = read_input_with_esc(
                            &format!("Enter {} (comma separated, Esc to cancel)", field),
                            &existing,
                        )?;
                        match val {
                            Some(v) if !v.is_empty() => {
                                let items: Vec<String> = v
                                    .split(',')
                                    .map(|s| s.trim().to_string())
                                    .filter(|s| !s.is_empty())
                                    .collect();
                                set_plugin_list_field(&mut doc, &selected_repo_url, field, items)?;
                                modified = true;
                            }
                            _ => return Ok(false),
                        }
                    }
                    "rev" => {
                        let existing = current_plugin
                            .as_ref()
                            .and_then(|p| p.rev.clone())
                            .unwrap_or_default();
                        let val = read_input_with_esc(
                            "Enter rev (branch/tag/hash, Esc to cancel)",
                            &existing,
                        )?;
                        match val {
                            Some(v) if !v.is_empty() => {
                                update_plugin_config(
                                    &mut doc,
                                    &selected_repo_url,
                                    None,
                                    None,
                                    None,
                                    None,
                                    Some(v),
                                )?;
                                modified = true;
                            }
                            _ => return Ok(false),
                        }
                    }
                    _ => {}
                }
            }
            None => return Ok(false),
        }
    }

    if modified {
        let chezmoi_enabled = read_chezmoi_flag(&config_path);
        chezmoi::write_routed(chezmoi_enabled, &config_path, doc.to_string()).await?;
        println!("Updated config for: {}", selected_repo_url);
        return Ok(true);
    }
    Ok(false)
}

pub(crate) async fn run_remove(query: Option<String>) -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    let Some(selected_url) =
        select_plugin_url(&config.plugins, query.as_deref(), "Select plugin to remove")?
    else {
        return Ok(());
    };

    let confirm = dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(format!("Remove plugin '{}'?", selected_url))
        .default(false)
        .interact()?;

    if !confirm {
        println!("Cancelled.");
        return Ok(());
    }

    let mut doc = toml_content.parse::<DocumentMut>()?;
    remove_plugin_from_toml(&mut doc, &selected_url)?;
    let chezmoi_enabled = read_chezmoi_flag(&config_path);
    chezmoi::write_routed(chezmoi_enabled, &config_path, doc.to_string()).await?;
    println!("Removed '{}' from config.", selected_url);

    let cache_root = resolve_cache_root(config.options.cache_root.as_deref());
    let plugin = config
        .plugins
        .iter()
        .find(|p| p.url == selected_url)
        .unwrap();
    let dst_path = resolve_plugin_dst(plugin, &cache_root);

    if dst_path.exists() {
        std::fs::remove_dir_all(&dst_path)?;
        println!("Deleted directory: {}", dst_path.display());
    }

    println!("Regenerating loader.lua...");
    run_generate(false).await?;
    Ok(())
}

pub(crate) async fn run_tune(
    query: Option<String>,
    ai_override: Option<crate::config::AiBackend>,
) -> Result<()> {
    let config_path = rvpm_config_path();
    ensure_config_exists(&config_path)?;
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    if config.plugins.is_empty() {
        return Err(anyhow::anyhow!(
            "No plugins in config.toml. Use `rvpm add <repo>` first."
        ));
    }

    // AI backend を解決。`--no-ai` (Off) や config が Off なら error
    // (tune は AI 専用 — non-AI 経路を提供する意味がない、`set` で代替できる)。
    let effective_ai = ai_override.unwrap_or(config.options.ai);
    let backend = crate::ai::Backend::try_from(effective_ai).map_err(|_| {
        anyhow::anyhow!(
            "rvpm tune requires an AI backend. Set `options.ai` in config.toml \
             or pass `--ai <claude|gemini|codex>`."
        )
    })?;

    let Some(selected_url) =
        select_plugin_url(&config.plugins, query.as_deref(), "Select plugin to tune")?
    else {
        return Ok(());
    };

    let plugin = config
        .plugins
        .iter()
        .find(|p| p.url == selected_url)
        .cloned()
        .context("plugin disappeared after selection")?;

    let cache_root = resolve_cache_root(config.options.cache_root.as_deref());
    let dst_path = resolve_plugin_dst(&plugin, &cache_root);
    if !dst_path.exists() {
        return Err(anyhow::anyhow!(
            "Plugin directory does not exist: {}. Run `rvpm sync` first so the AI can read the README/doc.",
            dst_path.display()
        ));
    }

    // 現在の `[[plugins]]` entry を TOML テキストとして抜き出す。
    let doc = toml_content.parse::<DocumentMut>()?;
    let current_entry_toml = extract_plugin_entry_toml(&doc, &selected_url).ok_or_else(|| {
        anyhow::anyhow!("could not extract current entry for `{selected_url}` from config.toml")
    })?;

    let config_root = resolve_config_root(config.options.config_root.as_deref());
    let plugin_cfg_dir = resolve_plugin_config_dir(&config_root, &plugin);

    println!(
        "\u{1f527} Tuning {} with {} ...",
        plugin.display_name(),
        backend.label()
    );

    match crate::ai::run_ai_tune(
        backend,
        &selected_url,
        &dst_path,
        &plugin_cfg_dir,
        &config_root,
        &config_path,
        &current_entry_toml,
        &config.options.ai_language,
        config.options.chezmoi,
    )
    .await
    {
        Ok(outcome) => match outcome.outcome {
            crate::ai::ChatOutcome::Applied { hook_changes } => {
                // user が `[[plugins]]` セクションで "Keep existing entry" を選んだら
                // `plugin_entry_toml` は None — config.toml は触らず、hook ファイル更新のみ。
                if let Some(entry_toml) = outcome.plugin_entry_toml {
                    let latest = std::fs::read_to_string(&config_path)?;
                    let mut doc_patch = latest.parse::<DocumentMut>()?;
                    // user は preview で fresh / merged を per-section に選択済み。
                    // `Replace` mode で AI が omit した stale field (e.g. 古い `on_cmd`) を消す。
                    if let Err(e) = replace_plugin_entry_with_ai_toml(
                        &mut doc_patch,
                        &selected_url,
                        &entry_toml,
                        &[],
                        MergeMode::Replace,
                    ) {
                        eprintln!(
                            "\u{26a0} failed to apply AI proposal: {e}. Existing entry kept."
                        );
                    } else {
                        let patched = doc_patch.to_string();
                        chezmoi::write_routed(config.options.chezmoi, &config_path, &patched)
                            .await?;
                        println!(
                            "Tuned {} ({} hook(s) written, {} removed).",
                            plugin.display_name(),
                            hook_changes.written.len(),
                            hook_changes.removed.len()
                        );
                    }
                } else {
                    println!(
                        "Kept existing entry for {} ({} hook(s) written, {} removed).",
                        plugin.display_name(),
                        hook_changes.written.len(),
                        hook_changes.removed.len()
                    );
                }
            }
            crate::ai::ChatOutcome::Skipped => {
                eprintln!("AI proposal skipped \u{2014} existing entry kept in config.toml.");
            }
            crate::ai::ChatOutcome::HandedOff => {
                eprintln!(
                    "Handed off to {} CLI. rvpm exits \u{2014} that session controls config.toml from here.",
                    backend.label()
                );
            }
        },
        Err(e) => {
            eprintln!("\u{26a0} AI tune failed: {e:#}. Existing entry kept unchanged.");
            eprintln!(
                "\n  Debug knobs (env vars):\n\
                 \x20 RVPM_AI_DUMP_PROMPT=/tmp/p.md   write the prompt to a file and skip the AI call\n\
                 \x20 RVPM_AI_NO_MERGED=1             drop the `_merged` variant requirement (force off)\n\
                 \x20 RVPM_AI_FORCE_MERGED=1          force `_merged` on for Gemini (auto-disabled\n\
                 \x20                                 by default because gemini-cli v0.39's loop guard\n\
                 \x20                                 aborts on near-duplicate fresh+merged output)\n\
                 \x20 RVPM_AI_TIMEOUT_SECS=600        raise the per-call timeout (default 300)"
            );
        }
    }

    run_generate(false).await?;
    Ok(())
}

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
