mod chezmoi;
mod config;
mod external_render;
mod git;
mod link;
mod loader;
mod store;
mod store_tui;
mod tui;

use crate::config::parse_config;
use crate::git::Repo;
use crate::link::merge_plugin;
use crate::loader::generate_loader;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::task::JoinSet;

// Clap 4 styling: section headers / usage / literals / placeholders を色分けする。
// `const` で渡せるようにビルダ経由で作成 (clap 4.5+ は Styles::styled() が const)。
const CLI_STYLES: clap::builder::styling::Styles = {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
        .usage(AnsiColor::BrightGreen.on_default().effects(Effects::BOLD))
        .literal(AnsiColor::BrightBlue.on_default().effects(Effects::BOLD))
        .placeholder(AnsiColor::Magenta.on_default())
        .error(AnsiColor::BrightRed.on_default().effects(Effects::BOLD))
        .valid(AnsiColor::BrightGreen.on_default())
        .invalid(AnsiColor::BrightYellow.on_default())
};

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Fast Neovim plugin manager with pre-compiled loader and merge optimization",
    long_about = "\
rvpm clones plugins in parallel, links merge=true plugins into a single\n\
runtime-path entry, and pre-compiles a loader.lua that sources everything\n\
without runtime glob cost. Inspired by lazy.nvim but adds merge and\n\
ahead-of-time file-list compilation on top.\n\
\n\
Run `rvpm init --write` once after your first `rvpm sync` to wire the\n\
generated loader.lua into your Neovim init.lua.",
    styles = CLI_STYLES,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Clone/pull plugins and regenerate loader.lua
    ///
    /// With --prune, also delete any plugin directories under the repos
    /// cache that are no longer referenced by config.toml.
    Sync {
        /// Delete unused plugin directories after syncing
        #[arg(long)]
        prune: bool,
    },

    /// Regenerate loader.lua only (no git)
    ///
    /// Useful after editing per-plugin init/before/after.lua or tweaking
    /// TOML triggers — skips the clone/pull phase entirely.
    Generate,

    /// Delete plugin directories no longer referenced by config.toml
    ///
    /// Walks `{cache_root}/plugins/repos/` and removes every clone whose
    /// plugin is no longer in `config.toml`. Does not run git operations,
    /// so it is much faster than `sync --prune` on large configs
    /// (hundreds of plugins).
    Clean,

    /// Add a plugin and sync
    ///
    /// Accepts the same trigger flags as `set` to configure the plugin
    /// in one shot: `rvpm add owner/repo --on-cmd Foo`
    Add {
        /// Plugin repo: owner/repo, URL, or local path
        repo: String,

        /// Friendly name (optional)
        #[arg(long)]
        name: Option<String>,

        /// Set lazy flag
        #[arg(long)]
        lazy: Option<bool>,

        /// Set on_cmd. Comma-separated or JSON array.
        #[arg(long)]
        on_cmd: Option<String>,

        /// Set on_ft. Comma-separated or JSON array.
        #[arg(long)]
        on_ft: Option<String>,

        /// Set on_map. Comma-separated or JSON array/object.
        #[arg(long)]
        on_map: Option<String>,

        /// Set on_event. Comma-separated or JSON array.
        #[arg(long)]
        on_event: Option<String>,

        /// Set rev (branch/tag/commit)
        #[arg(long)]
        rev: Option<String>,
    },

    /// Edit per-plugin or global hook files in $EDITOR
    ///
    /// Without flags, prompts which plugin and file to edit.
    /// With --init / --before / --after, opens that file directly.
    /// With --global, edits global before.lua / after.lua hooks
    /// (~/.config/rvpm/) instead of per-plugin files.
    Edit {
        /// Fuzzy match plugin url (omit to pick interactively)
        query: Option<String>,

        /// Open init.lua directly (per-plugin only)
        #[arg(long)]
        init: bool,

        /// Open before.lua directly
        #[arg(long)]
        before: bool,

        /// Open after.lua directly
        #[arg(long)]
        after: bool,

        /// Edit global hooks instead of per-plugin files
        #[arg(long)]
        global: bool,
    },

    /// Tweak a plugin's options interactively
    ///
    /// Walks through lazy / merge / on_* / rev with fuzzy-select and
    /// ESC-cancellable prompts. Pick `[ Open config.toml in $EDITOR ]`
    /// to drop into raw TOML editing when you need table-form on_map
    /// or complex `cond` expressions.
    Set {
        /// Fuzzy match plugin url (omit to pick interactively)
        query: Option<String>,

        /// Set lazy flag non-interactively
        #[arg(long)]
        lazy: Option<bool>,

        /// Set merge flag non-interactively
        #[arg(long)]
        merge: Option<bool>,

        /// Set on_cmd. Comma-separated (`"Foo,Bar"`) or JSON array
        /// (`'["Foo","Bar"]'`).
        #[arg(long)]
        on_cmd: Option<String>,

        /// Set on_ft. Comma-separated or JSON array.
        #[arg(long)]
        on_ft: Option<String>,

        /// Set on_map. Comma-separated lhs list, JSON array of
        /// strings, or JSON array/object with full `{ lhs, mode, desc }`
        /// form. Example: --on-map '{"lhs":"<space>d","mode":["n","x"]}'
        #[arg(long)]
        on_map: Option<String>,

        /// Set on_event. Comma-separated or JSON array. Supports the
        /// `"User Xxx"` shorthand for User events with patterns.
        #[arg(long)]
        on_event: Option<String>,

        /// Set on_path glob list. Comma-separated or JSON array.
        #[arg(long)]
        on_path: Option<String>,

        /// Set on_source (plugin names). Comma-separated or JSON array.
        #[arg(long)]
        on_source: Option<String>,

        /// Set rev (branch/tag/commit) non-interactively
        #[arg(long)]
        rev: Option<String>,
    },

    /// Update (git pull) installed plugins
    Update {
        /// Fuzzy match plugin url (omit to update all)
        query: Option<String>,
    },

    /// Remove a plugin and delete its directory
    Remove {
        /// Fuzzy match plugin url (omit to pick interactively)
        query: Option<String>,
    },

    /// Show plugin list (TUI by default, plain text with --no-tui)
    ///
    /// TUI keys: [q] quit  [j/k] move  [e] edit  [s] set  [S] sync all
    /// [u] update selected  [U] update all  [g] regenerate  [d] remove{n}
    /// With --no-tui: prints a sorted plain-text status line per plugin
    /// (pipe-friendly for scripting).
    List {
        /// Print plain text instead of launching the TUI
        #[arg(long)]
        no_tui: bool,
    },

    /// Open config.toml in $EDITOR
    ///
    /// Runs `sync` automatically after the editor exits.
    Config,

    /// Print or write the init.lua loader snippet
    ///
    /// Without --write: prints the exact `dofile(vim.fn.expand("..."))`
    /// line for your current config. Copy it into your Neovim init.lua.
    ///
    /// With --write: appends the snippet to `$NVIM_APPNAME`'s init.lua
    /// (defaults to `~/.config/nvim/init.lua`). If init.lua does not
    /// exist it is created with a header comment. Idempotent — a no-op
    /// if the loader is already referenced.
    Init {
        /// Append to init.lua (creates the file if missing)
        #[arg(long)]
        write: bool,
    },
    /// Browse and install Neovim plugins from GitHub
    Store,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::List { no_tui: false }) {
        Commands::Sync { prune } => {
            run_sync(prune).await?;
        }
        Commands::Generate => {
            run_generate().await?;
        }
        Commands::Clean => {
            run_clean()?;
        }
        Commands::Add {
            repo,
            name,
            lazy,
            on_cmd,
            on_ft,
            on_map,
            on_event,
            rev,
        } => {
            run_add(repo, name, lazy, on_cmd, on_ft, on_map, on_event, rev).await?;
        }
        Commands::Edit {
            query,
            init,
            before,
            after,
            global,
        } => {
            if run_edit(query, init, before, after, global).await? {
                run_generate().await?;
            }
        }
        Commands::Set {
            query,
            lazy,
            merge,
            on_cmd,
            on_ft,
            on_map,
            on_event,
            on_path,
            on_source,
            rev,
        } => {
            if run_set(
                query, lazy, merge, on_cmd, on_ft, on_map, on_event, on_path, on_source, rev,
            )
            .await?
            {
                run_generate().await?;
            }
        }
        Commands::Update { query } => {
            run_update(query).await?;
        }
        Commands::Remove { query } => {
            run_remove(query).await?;
        }
        Commands::List { no_tui } => {
            run_list(no_tui).await?;
        }
        Commands::Config => {
            if run_config().await? {
                run_generate().await?;
            }
        }
        Commands::Init { write } => {
            run_init(write).await?;
        }
        Commands::Store => {
            run_store().await?;
        }
    }

    Ok(())
}

use crate::tui::{PluginStatus, TuiState};
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

/// cond + merge=true の組み合わせを検出し、silent に merge を無効化する。
/// (cond が false のとき merged rtp に中身が残ると矛盾するため。警告は
/// 出さない — 自明に整合させているだけで、ユーザーアクションは不要。)
fn disable_merge_if_cond(plugin: &mut crate::config::Plugin) {
    if plugin.cond.is_some() && plugin.merge {
        plugin.merge = false;
    }
}

/// プラグインの clone 先パスを解決する。
fn resolve_plugin_dst(plugin: &crate::config::Plugin, cache_root: &Path) -> PathBuf {
    if let Some(d) = &plugin.dst {
        PathBuf::from(d)
    } else {
        resolve_repos_dir(cache_root).join(plugin.canonical_path())
    }
}

/// プラグインの build コマンドを実行する (依存 rtp 解決込み)。
/// build が未設定なら None を返す。失敗時はエラーメッセージを返す。
async fn execute_build_command(
    plugin: &crate::config::Plugin,
    dst_path: &Path,
    config: &crate::config::Config,
    cache_root: &Path,
) -> Option<String> {
    let build_cmd = plugin.build.as_ref()?;
    let mut rtp_dirs = vec![dst_path.to_path_buf()];
    let mut visited = std::collections::HashSet::new();
    let mut stack: Vec<String> = plugin.depends.iter().flatten().cloned().collect();
    while let Some(dep) = stack.pop() {
        if !visited.insert(dep.clone()) {
            continue;
        }
        if let Some(dep_plugin) = config
            .plugins
            .iter()
            .find(|p| p.display_name() == dep || p.url == dep)
        {
            let dep_path = resolve_plugin_dst(dep_plugin, cache_root);
            rtp_dirs.push(dep_path);
            if let Some(deeper) = &dep_plugin.depends {
                stack.extend(deeper.clone());
            }
        }
    }
    let (prog, args) = parse_build_command(build_cmd, &rtp_dirs);
    let build_timeout = std::time::Duration::from_secs(300); // 5 minutes
    let mut child = match tokio::process::Command::new(&prog)
        .args(&args)
        .current_dir(dst_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Some(format!("build spawn failed: {}", e));
        }
    };
    match tokio::time::timeout(build_timeout, child.wait()).await {
        Ok(Ok(status)) if !status.success() => {
            Some(format!("build failed (exit code: {:?})", status.code()))
        }
        Ok(Err(e)) => Some(format!("build error: {}", e)),
        Err(_) => {
            let _ = child.kill().await;
            Some(format!("build timed out ({}s)", build_timeout.as_secs()))
        }
        _ => None,
    }
}

async fn run_sync(prune: bool) -> Result<()> {
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
            let repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
            let res = repo.sync().await;
            match res {
                Ok(_) => {
                    if plugin.build.is_some() {
                        let _ = tx
                            .send((
                                plugin.url.clone(),
                                PluginStatus::Syncing(format!(
                                    "Building: {}",
                                    plugin.build.as_deref().unwrap_or_default()
                                )),
                            ))
                            .await;
                    }
                    let build_warn =
                        execute_build_command(&plugin, &dst_path, &config_for_build, &cache_root)
                            .await;
                    if let Some(ref err) = build_warn {
                        let _ = tx
                            .send((
                                plugin.url.clone(),
                                PluginStatus::Syncing(format!("Build warning: {}", err)),
                            ))
                            .await;
                    }
                    let _ = tx.send((plugin.url.clone(), PluginStatus::Finished)).await;
                    Ok((plugin, dst_path, build_warn))
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
    let config_root = resolve_config_root(config.options.config_root.as_deref());
    for plugin in config.plugins.iter().filter(|p| p.dev) {
        let dst_path = resolve_plugin_dst(plugin, &cache_root);
        let plugin_config_dir = resolve_plugin_config_dir(&config_root, plugin);
        if plugin.merge && !plugin.lazy {
            let _ = merge_plugin(&dst_path, &merged_dir);
        }
        plugin_scripts.push(build_plugin_scripts(plugin, &dst_path, &plugin_config_dir));
    }

    let mut build_warnings: Vec<(String, String)> = Vec::new();
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
                if let Ok(Ok((plugin, dst_path, build_warn))) = res {
                    if let Some(warn) = build_warn {
                        build_warnings.push((plugin.url.clone(), warn));
                    }
                    // lazy プラグインは merge しない (trigger 前に merged/ 経由で
                    // lua モジュールが rtp に漏れて lazy の意味がなくなるため)
                    if plugin.merge && !plugin.lazy {
                        let _ = merge_plugin(&dst_path, &merged_dir);
                    }
                    let config_root = resolve_config_root(config.options.config_root.as_deref());
                    let plugin_config_dir = resolve_plugin_config_dir(&config_root, &plugin);
                    let scripts = build_plugin_scripts(&plugin, &dst_path, &plugin_config_dir);
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
        for ps in &plugin_scripts {
            if promoted.contains(&ps.name) && ps.merge {
                let dst = PathBuf::from(&ps.path);
                let _ = merge_plugin(&dst, &merged_dir);
            }
        }
    }

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
    println!("Generating loader.lua...");
    let loader_path = resolve_loader_path(&cache_root);
    write_loader_to_path(
        &merged_dir,
        &plugin_scripts,
        &loader_path,
        &build_loader_options(&config_root),
    )?;
    println!("Done! -> {}", loader_path.display());

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

    print_init_lua_hint_if_missing(&config);
    Ok(())
}

/// `rvpm clean` — git 操作なしで、config.toml に無いプラグインディレクトリだけを削除する。
/// プラグイン数が多い環境で `sync --prune` が重いケースの受け皿。
/// 非同期処理は無いので `async` は付けない (clippy::unused_async 回避)。
fn run_clean() -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
    let config = parse_config(&toml_content)?;

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
    Ok(())
}

async fn run_generate() -> Result<()> {
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

    let mut plugin_scripts = Vec::new();
    let config_root = resolve_config_root(config.options.config_root.as_deref());
    for plugin in &config.plugins {
        let dst_path = resolve_plugin_dst(plugin, &cache_root);
        let plugin_config_dir = resolve_plugin_config_dir(&config_root, plugin);
        plugin_scripts.push(build_plugin_scripts(plugin, &dst_path, &plugin_config_dir));
    }

    // lazy → eager 昇格を適用。generate 単独実行時は merged/ が stale な可能性が
    // あるため、全 eager + merge プラグインを再構築する。
    crate::loader::promote_lazy_to_eager(&mut plugin_scripts);
    if merged_dir.exists() {
        let _ = std::fs::remove_dir_all(&merged_dir);
    }
    std::fs::create_dir_all(&merged_dir)?;
    for ps in &plugin_scripts {
        if !ps.lazy && ps.merge {
            let dst = PathBuf::from(&ps.path);
            if dst.exists() {
                let _ = merge_plugin(&dst, &merged_dir);
            }
        }
    }

    println!("Generating loader.lua...");
    write_loader_to_path(
        &merged_dir,
        &plugin_scripts,
        &loader_path,
        &build_loader_options(&config_root),
    )?;
    println!("Done! -> {}", loader_path.display());

    // `options.auto_clean = true` なら config から外されたプラグインディレクトリも
    // 自動削除 (git 操作は行わないので generate 自体のコストは増えない)。
    if config.options.auto_clean {
        let _ = maybe_prune_unused_repos(&config, &cache_root, true);
    }

    print_init_lua_hint_if_missing(&config);
    Ok(())
}

/// 全プラグインの git 状態を並列で調べ、url -> PluginStatus のマップを返す。
/// 全プラグインのステータスチェックを並列で spawn し、受信用 channel と
/// JoinSet を返す。呼び出し側は progressive に受信して描画するか、一括で
/// await して完了を待つか選べる。
fn spawn_status_check(
    config: &config::Config,
    cache_root: &Path,
) -> (mpsc::Receiver<(String, PluginStatus)>, JoinSet<()>) {
    let (tx, rx) = mpsc::channel::<(String, PluginStatus)>(100);
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
    config: &config::Config,
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

async fn run_list(no_tui: bool) -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let mut config = parse_config(&toml_content)?;
    let cache_root = resolve_cache_root(config.options.cache_root.as_deref());
    let config_root = resolve_config_root(config.options.config_root.as_deref());
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
        return Ok(());
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
    let mut tui_state = TuiState::new(urls);

    // バックグラウンドでステータスチェック開始 (TUI は即表示)
    let (mut rx, mut set) = spawn_status_check(&config, &cache_root);
    let mut bg_done = false;

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
            let (c, s, new_rx, new_set) =
                reload_state(&config_path, &cache_root, &mut terminal, &icons)?;
            icons = crate::tui::Icons::from_style(c.options.icons);
            config = c;
            tui_state = s;
            rx = new_rx;
            set = new_set;
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
    );
    fn reload_state(
        config_path: &Path,
        cache_root: &Path,
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
        let toml_content = std::fs::read_to_string(config_path)?;
        let config = parse_config(&toml_content)?;
        let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
        let tui_state = TuiState::new(urls);
        let (rx, set) = spawn_status_check(&config, cache_root);

        // ── 3. 復帰直後に残留イベントを drain ──
        // wait_for_keypress で押したキーの release や連打分が残ると、main
        // loop の最初の poll() で拾われて意図しないアクションが起動して
        // しまうことがあるため、ここで捨てる。
        while crossterm::event::poll(std::time::Duration::ZERO).unwrap_or(false) {
            let _ = crossterm::event::read();
        }

        Ok((config, tui_state, rx, set))
    }

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

        terminal.draw(|f| tui_state.draw_list(f, &config, &config_root, &icons))?;

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
                crossterm::event::KeyCode::Char('e') => {
                    if let Some(url) = tui_state.selected_url() {
                        leave_tui(&mut terminal)?;
                        if run_edit(Some(url), false, false, false, false).await? {
                            run_generate().await?;
                        }
                        reload!();
                    }
                }
                crossterm::event::KeyCode::Char('s') => {
                    if let Some(url) = tui_state.selected_url() {
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
                            run_generate().await?;
                        }
                        reload!();
                    }
                }
                crossterm::event::KeyCode::Char('S') => {
                    leave_tui(&mut terminal)?;
                    let _ = run_sync(false).await;
                    wait_for_keypress("\nPress any key to return to list...")?;
                    reload!();
                }
                crossterm::event::KeyCode::Char('u') => {
                    if let Some(url) = tui_state.selected_url() {
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
    Ok(())
}

async fn run_update(query: Option<String>) -> Result<()> {
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
            let dst_path = resolve_plugin_dst(&plugin, &cache_root);
            let _ = tx
                .send((
                    plugin.url.clone(),
                    PluginStatus::Syncing("Updating...".to_string()),
                ))
                .await;
            let repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
            let res = repo.update().await;
            match res {
                Ok(_) => {
                    let _ = tx.send((plugin.url.clone(), PluginStatus::Finished)).await;
                    Ok(())
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

    drop(tx);

    let total_tasks = target_plugins.len();
    let mut finished_tasks = 0;

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
            Some(_) = set.join_next() => { finished_tasks += 1; }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }
    terminal.draw(|f| tui_state.draw(f, "updating...", &icons))?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    println!("Update complete. Regenerating loader.lua...");
    run_generate().await?;
    Ok(())
}

use toml_edit::{DocumentMut, Item, table, value};

#[allow(clippy::too_many_arguments)]
async fn run_add(
    repo: String,
    name: Option<String>,
    lazy: Option<bool>,
    on_cmd: Option<String>,
    on_ft: Option<String>,
    on_map: Option<String>,
    on_event: Option<String>,
    rev: Option<String>,
) -> Result<()> {
    let config_path = rvpm_config_path();
    ensure_config_exists(&config_path)?;
    let toml_content = std::fs::read_to_string(&config_path)?;
    let mut doc = toml_content.parse::<DocumentMut>()?;
    if doc.get("plugins").is_none() {
        doc["plugins"] = toml_edit::ArrayOfTables::new().into();
    }
    let plugins = doc["plugins"]
        .as_array_of_tables_mut()
        .context("plugins is not an array of tables")?;
    for p in plugins.iter() {
        if p.get("url").and_then(|v| v.as_str()) == Some(&repo) {
            println!("Plugin already exists: {}", repo);
            return Ok(());
        }
    }
    let mut new_plugin = table();
    new_plugin["url"] = value(&repo);
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
    // on_* フラグがあれば set_plugin_list_field / set_plugin_map_field で追加
    let maybe_parse = |raw: Option<String>| -> Result<Option<Vec<String>>> {
        raw.map(|s| parse_cli_string_list(&s)).transpose()
    };
    if let Some(items) = maybe_parse(on_cmd)? {
        set_plugin_list_field(&mut doc, &repo, "on_cmd", items)?;
    }
    if let Some(items) = maybe_parse(on_ft)? {
        set_plugin_list_field(&mut doc, &repo, "on_ft", items)?;
    }
    if let Some(raw) = on_map {
        let specs = parse_on_map_cli(&raw)?;
        set_plugin_map_field(&mut doc, &repo, specs)?;
    }
    if let Some(items) = maybe_parse(on_event)? {
        set_plugin_list_field(&mut doc, &repo, "on_event", items)?;
    }

    let toml_content = doc.to_string();
    let chezmoi_enabled = read_chezmoi_flag(&config_path);
    let wp = chezmoi::write_path(chezmoi_enabled, &config_path);
    std::fs::write(&wp, &toml_content)?;
    chezmoi::apply(&wp, &config_path);
    println!("Added plugin to config: {}", repo);

    // 追加したプラグインだけ clone + merge し、loader.lua を再生成する
    let config_data = parse_config(&toml_content)?;
    let cache_root = resolve_cache_root(config_data.options.cache_root.as_deref());
    let merged_dir = resolve_merged_dir(&cache_root);

    if let Some(mut plugin) = config_data.plugins.iter().find(|p| p.url == repo).cloned() {
        disable_merge_if_cond(&mut plugin);
        let dst_path = resolve_plugin_dst(&plugin, &cache_root);

        println!("Syncing {}...", plugin.display_name());
        let git_repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
        if let Err(e) = git_repo.sync().await {
            eprintln!("Warning: failed to sync '{}': {}", plugin.display_name(), e);
        } else {
            if let Some(err) =
                execute_build_command(&plugin, &dst_path, &config_data, &cache_root).await
            {
                eprintln!("Warning: {}: {}", plugin.display_name(), err);
            }

            if plugin.merge && !plugin.lazy {
                std::fs::create_dir_all(&merged_dir).ok();
                let _ = merge_plugin(&dst_path, &merged_dir);
            }
        }
    }

    run_generate().await?;
    Ok(())
}

use dialoguer::{FuzzySelect, Select};

/// `rvpm config` — config.toml を $EDITOR で直接開く。
/// ファイルが無ければテンプレートで自動作成してから開く。
/// 常に `Ok(true)` を返すので呼び出し側で sync を走らせる前提。
async fn run_config() -> Result<bool> {
    let config_path = rvpm_config_path();
    ensure_config_exists(&config_path)?;
    let chezmoi_enabled = read_chezmoi_flag(&config_path);
    let edit_target = chezmoi::write_path(chezmoi_enabled, &config_path);
    println!("Opening {}", edit_target.display());
    open_editor_at_line(&edit_target, 1)?;
    chezmoi::apply(&edit_target, &config_path);
    Ok(true)
}

/// `rvpm init` — Neovim init.lua に loader.lua を繋ぐ dofile 行を案内 or 自動追記する。
async fn run_init(write: bool) -> Result<()> {
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
        let wp = chezmoi::write_path(config.options.chezmoi, &init_lua_path);
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
            chezmoi::apply(&wp, &init_lua_path);
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

async fn run_edit(
    query: Option<String>,
    flag_init: bool,
    flag_before: bool,
    flag_after: bool,
    flag_global: bool,
) -> Result<bool> {
    // --global: グローバル hooks (<config_root>/before.lua / after.lua)
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

        let file_name = if flag_before {
            "before.lua"
        } else if flag_after {
            "after.lua"
        } else {
            let file_names = ["before.lua", "after.lua"];
            let display_items: Vec<String> = file_names
                .iter()
                .map(|f| file_with_icon(&config_dir, f))
                .collect();
            let sel = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                .with_prompt("Select global hook to edit (\u{25cf}=exists \u{25cb}=new)")
                .default(0)
                .items(&display_items)
                .interact_opt()?;
            match sel {
                Some(index) => file_names[index],
                None => return Ok(false),
            }
        };

        let target = config_dir.join(file_name);
        let chezmoi_enabled = read_chezmoi_flag(&config_path);
        let edit_target = chezmoi::write_path(chezmoi_enabled, &target);
        if let Some(parent) = edit_target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        println!("\n>> Editing global hook: {}", edit_target.display());
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
        std::process::Command::new(editor)
            .arg(&edit_target)
            .status()?;
        chezmoi::apply(&edit_target, &target);
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

        let global_indicators = hook_indicators(&config_dir);
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
    let edit_target = chezmoi::write_path(chezmoi_enabled, &target_file);
    if let Some(parent) = edit_target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
    std::process::Command::new(editor)
        .arg(&edit_target)
        .status()?;
    chezmoi::apply(&edit_target, &target_file);
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn run_set(
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

    let selected_repo_url = if let Some(q) = query.as_ref() {
        config
            .plugins
            .iter()
            .find(|p| &p.url == q || p.url.contains(q))
            .map(|p| p.url.clone())
            .context("Plugin not found")?
    } else {
        let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
        let selection = FuzzySelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Select plugin to set")
            .default(0)
            .items(&urls)
            .interact_opt()?;
        match selection {
            Some(index) => urls[index].clone(),
            None => return Ok(false),
        }
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
                        let ep = chezmoi::write_path(cz, &config_path);
                        open_editor_at_line(&ep, line)?;
                        chezmoi::apply(&ep, &config_path);
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
                                let ep = chezmoi::write_path(cz, &config_path);
                                open_editor_at_line(&ep, line)?;
                                chezmoi::apply(&ep, &config_path);
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
        let wp = chezmoi::write_path(chezmoi_enabled, &config_path);
        std::fs::write(&wp, doc.to_string())?;
        chezmoi::apply(&wp, &config_path);
        println!("Updated config for: {}", selected_repo_url);
        return Ok(true);
    }
    Ok(false)
}

/// 英語の単数/複数形切替。表示メッセージで使う小さなヘルパー。
fn plural<'a>(singular: &'a str, plural: &'a str, n: usize) -> &'a str {
    if n == 1 { singular } else { plural }
}

/// `sync --prune` / `generate` (auto_clean) / 両方の末尾で使う共通の「後片付け」。
/// 未使用 repo を検出し、`force` が true なら `prune_unused_repos` で削除する。
/// 戻り値は検出された未使用の件数。0 以外なら呼び出し側で警告メッセージを
/// 出せるよう、発見はしたが削除していないケースを区別できるようにする。
fn maybe_prune_unused_repos(
    config: &config::Config,
    cache_root: &Path,
    force: bool,
) -> (usize, Vec<PathBuf>) {
    let repos_dir = resolve_repos_dir(cache_root);
    if !repos_dir.exists() {
        return (0, Vec::new());
    }
    let unused = find_unused_repos(config, &repos_dir).unwrap_or_default();
    if unused.is_empty() {
        return (0, Vec::new());
    }
    let count = unused.len();
    if force {
        prune_unused_repos(&unused);
        (count, Vec::new()) // 削除済みなのでパスは返さない
    } else {
        (count, unused)
    }
}

/// 未使用 repo ディレクトリを削除する共通処理。`sync --prune` と `clean` 両方から呼ばれる。
/// 削除失敗は eprintln で警告のみ出し、処理を続ける (resilience 原則)。
fn prune_unused_repos(unused: &[PathBuf]) {
    println!();
    println!(
        "Pruning {} unused plugin {}:",
        unused.len(),
        plural("directory", "directories", unused.len()),
    );
    for path in unused {
        println!("  - {}", path.display());
        if let Err(e) = std::fs::remove_dir_all(path) {
            eprintln!("    \u{26a0} failed: {}", e);
        }
    }
}

fn find_unused_repos(config: &config::Config, repos_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut unused = Vec::new();
    let mut used_paths = std::collections::HashSet::new();
    for plugin in &config.plugins {
        used_paths.insert(repos_dir.join(plugin.canonical_path()));
    }
    for entry in walkdir::WalkDir::new(repos_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() == ".git")
    {
        let git_dir = entry.path();
        if let Some(repo_root) = git_dir.parent()
            && !used_paths.contains(repo_root)
        {
            unused.push(repo_root.to_path_buf());
        }
    }
    Ok(unused)
}

fn remove_plugin_from_toml(doc: &mut DocumentMut, url: &str) -> Result<()> {
    let plugins = doc["plugins"]
        .as_array_of_tables_mut()
        .context("plugins is not an array of tables")?;
    let idx = plugins
        .iter()
        .position(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
        .context("Plugin not found in config")?;
    plugins.remove(idx);
    Ok(())
}

async fn run_remove(query: Option<String>) -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    let selected_url = if let Some(q) = query.as_ref() {
        config
            .plugins
            .iter()
            .find(|p| p.url == *q || p.url.contains(q.as_str()))
            .map(|p| p.url.clone())
            .context("Plugin not found")?
    } else {
        let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
        let selection = FuzzySelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Select plugin to remove")
            .default(0)
            .items(&urls)
            .interact_opt()?;
        match selection {
            Some(idx) => urls[idx].clone(),
            None => return Ok(()),
        }
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
    let wp = chezmoi::write_path(chezmoi_enabled, &config_path);
    std::fs::write(&wp, doc.to_string())?;
    chezmoi::apply(&wp, &config_path);
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
    run_generate().await?;
    Ok(())
}

/// 指定プラグインの任意のリスト型フィールド (on_cmd / on_ft / on_map / on_event / on_path / on_source 等) を設定する。
/// 要素が1つの場合は文字列として、2つ以上の場合は配列として書き込む (TOML の string | string[] を活用)。
fn set_plugin_list_field(
    doc: &mut DocumentMut,
    url: &str,
    field: &str,
    values: Vec<String>,
) -> Result<()> {
    let plugins = doc["plugins"]
        .as_array_of_tables_mut()
        .context("plugins is not an array of tables")?;
    let plugin_table = plugins
        .iter_mut()
        .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
        .context("Could not find plugin in toml_edit document")?;
    if values.len() == 1 {
        plugin_table[field] = value(values.into_iter().next().unwrap());
    } else {
        let mut array = toml_edit::Array::new();
        for v in values {
            array.push(v);
        }
        plugin_table[field] = value(array);
    }
    Ok(())
}

/// `--on-cmd` / `--on-ft` / `--on-event` / `--on-path` / `--on-source` の
/// 入力文字列を `Vec<String>` に正規化する。
///
/// 受け付ける形式:
/// - `"Foo"`                 → `["Foo"]`
/// - `"Foo,Bar,Baz"`         → `["Foo", "Bar", "Baz"]` (空要素は無視)
/// - `'["Foo", "Bar"]'`      → `["Foo", "Bar"]` (JSON 配列)
///
/// JSON っぽく `[` で始まっていて parse に失敗すると明示エラー。
fn parse_cli_string_list(input: &str) -> Result<Vec<String>> {
    let trimmed = input.trim();
    if trimmed.starts_with('[') {
        return serde_json::from_str::<Vec<String>>(trimmed)
            .with_context(|| format!("invalid JSON string array: {}", trimmed));
    }
    Ok(trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// `--on-map` CLI flag の入力を `Vec<MapSpec>` に変換する。
///
/// 受け付ける形式 (すべて同じ flag で混在可能):
/// - `"<leader>f"`                       (単純な文字列)
/// - `"<leader>f, <leader>g"`            (カンマ区切り)
/// - `'["<leader>f", "<leader>g"]'`      (JSON 文字列配列)
/// - `'{ "lhs": "<space>d", "mode": ["n", "x"], "desc": "..." }'`  (JSON object 単体)
/// - `'[{ ... }, "<leader>f", { ... }]'`  (JSON mixed array)
fn parse_on_map_cli(input: &str) -> Result<Vec<crate::config::MapSpec>> {
    let trimmed = input.trim();
    let first = trimmed.chars().next().unwrap_or(' ');

    // JSON 解析を試みる (配列 or オブジェクト先頭)
    if first == '[' || first == '{' {
        let value: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("invalid JSON for --on-map: {}", trimmed))?;
        return match value {
            serde_json::Value::Array(items) => items
                .into_iter()
                .map(map_spec_from_json_value)
                .collect::<Result<Vec<_>>>(),
            serde_json::Value::Object(_) => Ok(vec![map_spec_from_json_value(value)?]),
            _ => anyhow::bail!("--on-map JSON must be an object or array"),
        };
    }

    // 単純: カンマ区切り (空要素は無視) → 全部 lhs のみの MapSpec
    Ok(trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|lhs| crate::config::MapSpec {
            lhs,
            mode: Vec::new(),
            desc: None,
        })
        .collect())
}

fn map_spec_from_json_value(value: serde_json::Value) -> Result<crate::config::MapSpec> {
    use crate::config::MapSpec;
    match value {
        serde_json::Value::String(lhs) => Ok(MapSpec {
            lhs,
            mode: Vec::new(),
            desc: None,
        }),
        serde_json::Value::Object(map) => {
            let lhs = map
                .get("lhs")
                .and_then(|v| v.as_str())
                .map(String::from)
                .context("map spec missing required `lhs` field")?;
            let mode = match map.get("mode") {
                Some(serde_json::Value::String(s)) => vec![s.clone()],
                Some(serde_json::Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
                Some(_) => anyhow::bail!("`mode` must be a string or array of strings"),
                None => Vec::new(),
            };
            let desc = map.get("desc").and_then(|v| v.as_str()).map(String::from);
            Ok(MapSpec { lhs, mode, desc })
        }
        _ => anyhow::bail!("map spec must be a string or object"),
    }
}

/// `Vec<MapSpec>` を TOML の `on_map` フィールドに書き込む。
/// - 1 要素かつ simple (mode/desc なし) → plain string
/// - それ以外 → 配列 (要素ごとに simple なら string、詳細なら inline table)
fn set_plugin_map_field(
    doc: &mut DocumentMut,
    url: &str,
    specs: Vec<crate::config::MapSpec>,
) -> Result<()> {
    let plugins = doc["plugins"]
        .as_array_of_tables_mut()
        .context("plugins is not an array of tables")?;
    let plugin_table = plugins
        .iter_mut()
        .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
        .context("Could not find plugin in toml_edit document")?;

    let is_simple = |s: &crate::config::MapSpec| s.mode.is_empty() && s.desc.is_none();

    if specs.len() == 1 && is_simple(&specs[0]) {
        plugin_table["on_map"] = value(specs.into_iter().next().unwrap().lhs);
        return Ok(());
    }

    let mut array = toml_edit::Array::new();
    for spec in specs {
        if is_simple(&spec) {
            array.push(spec.lhs);
        } else {
            let mut inline = toml_edit::InlineTable::new();
            inline.insert("lhs", spec.lhs.into());
            if !spec.mode.is_empty() {
                let mut mode_arr = toml_edit::Array::new();
                for m in spec.mode {
                    mode_arr.push(m);
                }
                inline.insert("mode", toml_edit::Value::Array(mode_arr));
            }
            if let Some(desc) = spec.desc {
                inline.insert("desc", desc.into());
            }
            array.push(toml_edit::Value::InlineTable(inline));
        }
    }
    plugin_table["on_map"] = value(array);
    Ok(())
}

fn update_plugin_config(
    doc: &mut DocumentMut,
    url: &str,
    lazy: Option<bool>,
    merge: Option<bool>,
    on_cmd: Option<Vec<String>>,
    on_ft: Option<Vec<String>>,
    rev: Option<String>,
) -> Result<()> {
    if let Some(l) = lazy {
        let plugins = doc["plugins"]
            .as_array_of_tables_mut()
            .context("plugins is not an array of tables")?;
        let plugin_table = plugins
            .iter_mut()
            .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
            .context("Could not find plugin in toml_edit document")?;
        plugin_table["lazy"] = value(l);
    }
    if let Some(m) = merge {
        let plugins = doc["plugins"]
            .as_array_of_tables_mut()
            .context("plugins is not an array of tables")?;
        let plugin_table = plugins
            .iter_mut()
            .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
            .context("Could not find plugin in toml_edit document")?;
        plugin_table["merge"] = value(m);
    }
    if let Some(cmds) = on_cmd {
        set_plugin_list_field(doc, url, "on_cmd", cmds)?;
    }
    if let Some(fts) = on_ft {
        set_plugin_list_field(doc, url, "on_ft", fts)?;
    }
    if let Some(r) = rev {
        let plugins = doc["plugins"]
            .as_array_of_tables_mut()
            .context("plugins is not an array of tables")?;
        let plugin_table = plugins
            .iter_mut()
            .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
            .context("Could not find plugin in toml_edit document")?;
        plugin_table["rev"] = value(r);
    }
    Ok(())
}

/// `<config_root>/before.lua` / `after.lua` を検出して LoaderOptions を構築する。
fn build_loader_options(config_root: &Path) -> crate::loader::LoaderOptions {
    crate::loader::LoaderOptions {
        global_before: find_lua(config_root, "before.lua"),
        global_after: find_lua(config_root, "after.lua"),
    }
}

fn write_loader_to_path(
    merged_dir: &Path,
    scripts: &[crate::loader::PluginScripts],
    loader_path: &Path,
    loader_opts: &crate::loader::LoaderOptions,
) -> Result<()> {
    if let Some(parent) = loader_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lua = generate_loader(merged_dir, scripts, loader_opts);
    std::fs::write(loader_path, lua)?;
    Ok(())
}

/// デフォルト並列数。GitHub の rate limit を避けるため控えめに。
const DEFAULT_CONCURRENCY: usize = 8;

fn resolve_concurrency(config_value: Option<usize>) -> usize {
    config_value.unwrap_or(DEFAULT_CONCURRENCY)
}

// ====================================================================
// Paths: `.config` / `.cache` をクロスプラットフォームで固定する。
//
// Windows でも `dirs::config_dir()` (≒ `%APPDATA%`) ではなく明示的に
// `~/.config` / `~/.cache` を使う。理由:
//   - Neovim の config 慣習と一致 (`~/.config/nvim`)
//   - dotfiles を WSL / Linux / Windows で同じパス構造で共有できる
//   - 単一の mental model で済む
//
// ユーザー側で別のパスにしたければ TOML の options で上書きできる:
//   - options.cache_root  → 全キャッシュの root (plugins/ と store/ が配下)
//   - options.config_root → 全コンフィグの root (config.toml / 全 global hook /
//                           plugins/ が配下)
//
// config_root と cache_root は対称構造:
//   <config_root>/config.toml
//   <config_root>/before.lua / after.lua             (global hooks)
//   <config_root>/plugins/<host>/<owner>/<repo>/     (per-plugin hooks)
//   <cache_root>/plugins/{repos,merged,loader.lua}   (plugins 本体)
//   <cache_root>/store/                              (store キャッシュ)
//
// $RVPM_APPNAME > $NVIM_APPNAME > "nvim" の順で appname が決まり、
// デフォルトパスの末尾に appname が入る:
//   ~/.config/rvpm/<appname>/
//   ~/.cache/rvpm/<appname>/
// ====================================================================

/// `~/.config/rvpm/config.toml` (固定)
/// $RVPM_APPNAME → $NVIM_APPNAME → "nvim" の優先順で appname を決定。
/// 無効な値 (空文字、パス区切り含む、"." / "..") は "nvim" に fallback。
pub(crate) fn appname() -> String {
    let raw = std::env::var("RVPM_APPNAME")
        .or_else(|_| std::env::var("NVIM_APPNAME"))
        .unwrap_or_default();
    if is_valid_appname(&raw) {
        raw
    } else {
        "nvim".to_string()
    }
}

/// appname が path segment として安全か検証。
fn is_valid_appname(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

fn rvpm_config_path() -> PathBuf {
    let home = dirs::home_dir().expect("Could not find home directory");
    home.join(".config")
        .join("rvpm")
        .join(appname())
        .join("config.toml")
}

/// `config.toml` から `options.chezmoi` フラグだけを軽量に読み出す。
/// `parse_config` は Tera 展開 + topological sort を行う重量級処理なので、
/// mutate 系コマンドがフラグ 1 つを見るためだけに呼ぶのは無駄。
/// toml_edit で該当キーだけ直接参照する。
/// ファイルが存在しない / パースできない / キーが無い場合は `false`。
fn read_chezmoi_flag(config_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(doc) = content.parse::<DocumentMut>() else {
        return false;
    };
    doc.get("options")
        .and_then(|o| o.get("chezmoi"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// config.toml が存在しなければ最小テンプレートで新規作成する。
/// 既に存在する場合は何もしない (冪等)。作成した場合は true を返す。
fn ensure_config_exists(config_path: &Path) -> Result<bool> {
    if config_path.exists() {
        return Ok(false);
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let template = "\
# rvpm config — https://github.com/yukimemi/rvpm#configuration
[options]
";
    std::fs::write(config_path, template)?;
    println!("Created {}", config_path.display());
    Ok(true)
}

/// `~` / `~/foo` / `~\foo` 形式を home dir に展開する。
/// それ以外はそのまま PathBuf に変換。
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().expect("Could not find home directory");
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return dirs::home_dir()
            .expect("Could not find home directory")
            .join(rest);
    }
    PathBuf::from(path)
}

/// rvpm のキャッシュ root を決定する。
/// `options.cache_root` が設定されていればそれを tilde 展開して返す。
/// 未設定なら `~/.cache/rvpm/<appname>` (デフォルト)。
/// この配下に `plugins/{repos,merged,loader.lua}` が配置される。
fn resolve_cache_root(config_cache_root: Option<&str>) -> PathBuf {
    match config_cache_root {
        Some(raw) => expand_tilde(raw),
        None => {
            let home = dirs::home_dir().expect("Could not find home directory");
            home.join(".cache").join("rvpm").join(appname())
        }
    }
}

/// config.toml / global hook / per-plugin hook の親 root を決定する。
/// `options.config_root` が設定されていればそれを tilde 展開して返す。
/// 未設定なら `~/.config/rvpm/<appname>` (デフォルト)。
fn resolve_config_root(config_root: Option<&str>) -> PathBuf {
    match config_root {
        Some(raw) => expand_tilde(raw),
        None => {
            let home = dirs::home_dir().expect("Could not find home directory");
            home.join(".config").join("rvpm").join(appname())
        }
    }
}

/// 指定プラグインの per-plugin hook ディレクトリ (`<config_root>/plugins/<host>/<owner>/<repo>`)
/// を返す。
fn resolve_plugin_config_dir(config_root: &Path, plugin: &config::Plugin) -> PathBuf {
    config_root.join("plugins").join(plugin.canonical_path())
}

/// loader.lua のパス。常に `<cache_root>/plugins/loader.lua`。
fn resolve_loader_path(cache_root: &Path) -> PathBuf {
    cache_root.join("plugins").join("loader.lua")
}

/// repos の親ディレクトリ。`<cache_root>/plugins/repos`。
fn resolve_repos_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("plugins").join("repos")
}

/// merged ディレクトリ。`<cache_root>/plugins/merged`。
fn resolve_merged_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("plugins").join("merged")
}

// ====================================================================
// rvpm init: Neovim init.lua に loader をつなぐためのヘルパー
// ====================================================================

/// `$NVIM_APPNAME` を考慮して init.lua のパスを返す (pure function、テスト容易性のため env は外から注入)。
fn nvim_init_lua_path_for_appname(appname: Option<&str>) -> PathBuf {
    let appname = appname.unwrap_or("nvim");
    let home = dirs::home_dir().expect("Could not find home directory");
    home.join(".config").join(appname).join("init.lua")
}

/// 実行時の `$NVIM_APPNAME` 環境変数を見て init.lua のパスを返す。
fn nvim_init_lua_path() -> PathBuf {
    let appname = std::env::var("NVIM_APPNAME").ok();
    nvim_init_lua_path_for_appname(appname.as_deref())
}

/// loader.lua を参照する `dofile(...)` 行を config から生成する。
/// 優先順位: `options.cache_root`/plugins/loader.lua > `~/.cache/rvpm/<appname>/plugins/loader.lua`
/// tilde 形式を保持することで dotfiles のマシン間共有を妨げない。
fn loader_init_snippet(config: &config::Config) -> String {
    let raw_path = if let Some(base) = &config.options.cache_root {
        format!("{}/plugins/loader.lua", base.trim_end_matches(['/', '\\']))
    } else {
        format!("~/.cache/rvpm/{}/plugins/loader.lua", appname())
    };
    // Windows のバックスラッシュを Lua 文字列リテラルで安全な '/' に正規化。
    let raw_path = raw_path.replace('\\', "/");
    format!("dofile(vim.fn.expand(\"{}\"))", raw_path)
}

/// init.lua が rvpm の loader を参照しているかを緩く検出する。
/// 同じ行内に `rvpm` と `loader.lua` が両方出ていれば真。
fn init_lua_references_rvpm_loader(init_lua_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(init_lua_path) else {
        return false;
    };
    content
        .lines()
        .any(|line| line.contains("rvpm") && line.contains("loader.lua"))
}

#[derive(Debug, PartialEq, Eq)]
enum WriteInitResult {
    /// init.lua が存在しなかったので新規作成した
    Created,
    /// 既存 init.lua に末尾追記した
    Appended,
    /// 既に loader を参照していて変更不要だった
    AlreadyConfigured,
}

/// init.lua に loader snippet を書き込む (冪等)。
fn write_init_lua_snippet(init_lua_path: &Path, snippet: &str) -> Result<WriteInitResult> {
    if init_lua_path.exists() {
        if init_lua_references_rvpm_loader(init_lua_path) {
            return Ok(WriteInitResult::AlreadyConfigured);
        }
        let mut content = std::fs::read_to_string(init_lua_path)?;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("\n-- rvpm loader (auto-added by `rvpm init --write`)\n");
        content.push_str(snippet);
        content.push('\n');
        std::fs::write(init_lua_path, content)?;
        Ok(WriteInitResult::Appended)
    } else {
        if let Some(parent) = init_lua_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = format!(
            "-- Neovim config (auto-created by `rvpm init --write`)\n\n-- rvpm loader\n{}\n",
            snippet
        );
        std::fs::write(init_lua_path, content)?;
        Ok(WriteInitResult::Created)
    }
}

/// `rvpm sync` / `rvpm generate` / `rvpm add` 等の末尾で呼ぶ hint 表示。
/// init.lua が loader を参照していない (or 未作成) なら案内を出す。
fn print_init_lua_hint_if_missing(config: &config::Config) {
    let init_lua_path = nvim_init_lua_path();
    if !init_lua_path.exists() {
        println!();
        println!(
            "\u{26a0} Neovim init.lua not found at {}",
            init_lua_path.display()
        );
        println!("  Run `rvpm init --write` to create one with the rvpm loader.");
        return;
    }
    if !init_lua_references_rvpm_loader(&init_lua_path) {
        let snippet = loader_init_snippet(config);
        println!();
        println!(
            "\u{26a0} {} doesn't reference rvpm loader yet.",
            init_lua_path.display()
        );
        println!("  Add this line:");
        println!("    {}", snippet);
        println!("  Or run `rvpm init --write` to do it automatically.");
    }
}

/// config.toml 上で指定プラグイン (url 一致) の `url = "..."` 行の行番号 (1-indexed) を返す。
/// 見つからなければ 1 を返す (ファイル先頭)。
/// whitespace の入り方に寛容: `url="..."`, `url = "..."`, `url  =   "..."` など全部拾う。
fn find_plugin_line_in_toml(toml_content: &str, url: &str) -> usize {
    let needle = format!("\"{}\"", url);
    for (i, line) in toml_content.lines().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("url") {
            continue;
        }
        // "url" の後は空白 or "=" しか来ないはず (他のフィールド名は "url..." で始まらない)
        let rest = trimmed["url".len()..].trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        if line.contains(&needle) {
            return i + 1;
        }
    }
    1
}

/// `$EDITOR` が `+<line>` 形式の行ジャンプをサポートするか簡易判定。
/// nvim/vim/vi/nano/emacs ファミリーは真。VS Code / helix 等は偽。
fn editor_supports_line_jump(editor_cmd: &str) -> bool {
    // Unix の Path は `\` をパス区切りと認識しないため、手動で両方で split する
    let file_name = editor_cmd.rsplit(['/', '\\']).next().unwrap_or(editor_cmd);
    let base = file_name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(file_name)
        .to_lowercase();
    matches!(base.as_str(), "nvim" | "vim" | "vi" | "nano" | "emacs")
}

/// `$EDITOR` (未設定なら "nvim") でファイルを開く。対応している editor なら指定行にジャンプ。
fn open_editor_at_line(path: &Path, line: usize) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
    let mut cmd = std::process::Command::new(&editor);
    if editor_supports_line_jump(&editor) {
        cmd.arg(format!("+{}", line));
    }
    cmd.arg(path);
    cmd.status()?;
    Ok(())
}

/// ESC キーで None を返し、Enter キーで入力文字列を Some で返すテキスト入力。
/// crossterm の raw mode を一時的に有効化して使用する。
/// `initial` を渡すと、その値を初期入力として表示・編集できる。
fn read_input_with_esc(prompt: &str, initial: &str) -> Result<Option<String>> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    use std::io::Write;

    let mut input = String::from(initial);
    print!("{}: {}", prompt, input);
    std::io::stdout().flush()?;

    crossterm::terminal::enable_raw_mode()?;

    let result = loop {
        match crossterm::event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => {
                    break Ok(None);
                }
                KeyCode::Enter => {
                    break Ok(Some(input.clone()));
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(anyhow::anyhow!("Interrupted"));
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    print!("{}", c);
                    std::io::stdout().flush()?;
                }
                KeyCode::Backspace if !input.is_empty() => {
                    input.pop();
                    print!("\x08 \x08");
                    std::io::stdout().flush()?;
                }
                _ => {}
            },
            _ => {}
        }
    };

    crossterm::terminal::disable_raw_mode()?;
    println!();
    result
}

/// init/before/after.lua の存在チェックしてサークルアイコンの文字列を返す
/// 例: "● ○ ●" (init あり、before なし、after あり)
fn hook_indicators(dir: &Path) -> String {
    let i = if dir.join("init.lua").exists() {
        "\u{25cf}"
    } else {
        "\u{25cb}"
    };
    let b = if dir.join("before.lua").exists() {
        "\u{25cf}"
    } else {
        "\u{25cb}"
    };
    let a = if dir.join("after.lua").exists() {
        "\u{25cf}"
    } else {
        "\u{25cb}"
    };
    format!("{} {} {}", i, b, a)
}

/// ファイル名に存在アイコンを付ける
fn file_with_icon(dir: &Path, name: &str) -> String {
    let icon = if dir.join(name).exists() {
        "\u{25cf}"
    } else {
        "\u{25cb}"
    };
    format!("{} {}", icon, name)
}

fn find_lua(dir: &Path, name: &str) -> Option<String> {
    let path = dir.join(name);
    if path.exists() {
        Some(path.to_string_lossy().to_string())
    } else {
        None
    }
}

/// 指定ディレクトリ配下を再帰的に walk し、`.vim` / `.lua` ファイルをソートして返す。
/// lazy.nvim の Util.walk + source_runtime のフィルタと同等。
/// ディレクトリが存在しない場合は空配列を返す (Resilience)。
/// `colors/` ディレクトリからカラースキーム名 (ファイル名から拡張子を除去) を収集する。
/// 例: `colors/catppuccin.lua` → `"catppuccin"`, `colors/catppuccin-latte.vim` → `"catppuccin-latte"`
/// build コマンドを解析して (実行プログラム, 引数リスト) を返す。
/// `:` で始まる場合は Neovim コマンドとして実行。rtp_dirs (自身 + 依存先) を
/// rtp に追加してコマンドや autoload 関数を使えるようにする。
/// それ以外はシェルコマンドとして `sh -c "..."` (Windows: `cmd /C "..."`) に変換。
fn parse_build_command(build_cmd: &str, rtp_dirs: &[PathBuf]) -> (String, Vec<String>) {
    if let Some(vim_cmd) = build_cmd.strip_prefix(':') {
        let rtp_cmds: Vec<String> = rtp_dirs
            .iter()
            .map(|d| format!("set rtp+={}", d.to_string_lossy().replace('\\', "/")))
            .collect();
        let rtp_cmd = rtp_cmds.join(" | ");
        (
            "nvim".to_string(),
            vec![
                "--headless".to_string(),
                "--cmd".to_string(),
                rtp_cmd,
                "-c".to_string(),
                vim_cmd.to_string(),
                "-c".to_string(),
                "qa!".to_string(),
            ],
        )
    } else if cfg!(windows) {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), build_cmd.to_string()],
        )
    } else {
        (
            "sh".to_string(),
            vec!["-c".to_string(), build_cmd.to_string()],
        )
    }
}

fn collect_colorschemes(plugin_path: &Path) -> Vec<String> {
    let dir = plugin_path.join("colors");
    if !dir.exists() {
        return Vec::new();
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let path = e.path();
            let ext = path.extension()?.to_str()?;
            if ext == "lua" || ext == "vim" {
                Some(path.file_stem()?.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

fn collect_source_files(plugin_path: &Path, subdir: &str) -> Vec<String> {
    let dir = plugin_path.join(subdir);
    if !dir.exists() {
        return Vec::new();
    }
    let mut files: Vec<String> = walkdir::WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext == "lua" || ext == "vim")
                .unwrap_or(false)
        })
        .map(|e| e.path().to_string_lossy().replace('\\', "/"))
        .collect();
    files.sort();
    files
}

/// Plugin の実ディスク情報から PluginScripts を構築するヘルパー。
/// run_sync / run_generate で重複していたロジックを集約。
fn build_plugin_scripts(
    plugin: &crate::config::Plugin,
    plugin_path: &Path,
    plugin_config_dir: &Path,
) -> crate::loader::PluginScripts {
    crate::loader::PluginScripts {
        name: plugin.display_name(),
        path: plugin_path.to_string_lossy().replace('\\', "/"),
        merge: plugin.merge,
        init: find_lua(plugin_config_dir, "init.lua"),
        before: find_lua(plugin_config_dir, "before.lua"),
        after: find_lua(plugin_config_dir, "after.lua"),
        plugin_files: collect_source_files(plugin_path, "plugin"),
        ftdetect_files: collect_source_files(plugin_path, "ftdetect"),
        after_plugin_files: collect_source_files(plugin_path, "after/plugin"),
        lazy: plugin.lazy,
        on_cmd: plugin.on_cmd.clone(),
        on_ft: plugin.on_ft.clone(),
        on_map: plugin.on_map.clone(),
        on_event: plugin.on_event.clone(),
        on_path: plugin.on_path.clone(),
        on_source: plugin.on_source.clone(),
        depends: plugin.depends.clone(),
        colorschemes: collect_colorschemes(plugin_path),
        cond: plugin.cond.clone(),
    }
}

/// `rvpm store` — GitHub からプラグインを検索して追加する TUI。
/// Plugin URL を GitHub の `owner/repo` 形式 (小文字) に正規化する。
/// GitHub の `full_name` は大文字小文字非依存なので lowercase で揃える。
/// - `"owner/repo"` → `Some("owner/repo")`
/// - `"https://github.com/Owner/Repo(.git)?"` → `Some("owner/repo")`
/// - `"git@github.com:Owner/Repo.git"` → `Some("owner/repo")`
/// - GitHub 以外 (gitlab 等) → None
fn installed_full_name(url: &str) -> Option<String> {
    // `.git` と末尾 `/` の順序が揺れるケースを両方受け付ける
    let trimmed = url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/');
    // SSH 形式: git@github.com:owner/repo
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 2 {
            return Some(format!("{}/{}", parts[0], parts[1]).to_lowercase());
        }
        return None;
    }
    // HTTPS/HTTP
    for prefix in ["https://github.com/", "http://github.com/"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let parts: Vec<&str> = rest.split('/').collect();
            if parts.len() >= 2 {
                return Some(format!("{}/{}", parts[0], parts[1]).to_lowercase());
            }
            return None;
        }
    }
    // 別ホストの URL は GitHub ではないので None
    if trimmed.contains("://") {
        return None;
    }
    // `owner/repo` 形式 (スキーム無し)
    if trimmed.contains('/') && !trimmed.contains(' ') {
        let parts: Vec<&str> = trimmed.split('/').collect();
        if parts.len() == 2 {
            return Some(format!("{}/{}", parts[0], parts[1]).to_lowercase());
        }
    }
    None
}

async fn run_store() -> Result<()> {
    use crate::store_tui::StoreTuiState;

    // cache_root / installed set / readme_command を config から解決。
    // resilience 原則: config.toml が壊れていても store TUI は defaults で開く。
    let config_path = rvpm_config_path();
    let defaults = || {
        (
            resolve_cache_root(None),
            std::collections::HashSet::<String>::new(),
            None::<Vec<String>>,
        )
    };
    let (cache_root, installed, readme_command) = 'resolve: {
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
                    .store
                    .readme_command
                    .filter(|v| !v.is_empty());
                (cache, set, cmd)
            }
            Err(e) => {
                eprintln!(
                    "\u{26a0} failed to parse {}: {}. Opening store with defaults.",
                    config_path.display(),
                    e
                );
                defaults()
            }
        }
    };

    let mut state = StoreTuiState::new();
    state.installed = installed;
    state.readme_command = readme_command;

    // 初期表示: 人気プラグインをバックグラウンドで取得
    let cache_root_bg = cache_root.clone();
    let popular = tokio::task::spawn_blocking(move || crate::store::fetch_popular(&cache_root_bg));

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
    // 外部 renderer (`options.store.readme_command`) の結果用チャネル。
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
                    let content = crate::store::fetch_readme(&cache_root_bg, &repo)
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
                            crate::store::search_plugins(&cache_root_bg, &query)
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
                        store_tui::Focus::List => state.next(),
                        store_tui::Focus::Readme => state.scroll_readme_down(1),
                    }
                }
                crossterm::event::KeyCode::Char('k') | crossterm::event::KeyCode::Up => {
                    match state.focus {
                        store_tui::Focus::List => state.previous(),
                        store_tui::Focus::Readme => state.scroll_readme_up(1),
                    }
                }
                crossterm::event::KeyCode::Char('g') | crossterm::event::KeyCode::Home => {
                    match state.focus {
                        store_tui::Focus::List => state.go_top(),
                        store_tui::Focus::Readme => state.readme_scroll = 0,
                    }
                }
                crossterm::event::KeyCode::Char('G') | crossterm::event::KeyCode::End => {
                    match state.focus {
                        store_tui::Focus::List => state.go_bottom(),
                        store_tui::Focus::Readme => state.scroll_readme_to_bottom(),
                    }
                }
                crossterm::event::KeyCode::Char('d')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    match state.focus {
                        store_tui::Focus::List => state.move_down(10),
                        store_tui::Focus::Readme => state.scroll_readme_down(10),
                    }
                }
                crossterm::event::KeyCode::Char('u')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    match state.focus {
                        store_tui::Focus::List => state.move_up(10),
                        store_tui::Focus::Readme => state.scroll_readme_up(10),
                    }
                }
                crossterm::event::KeyCode::Char('f')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    match state.focus {
                        store_tui::Focus::List => state.move_down(20),
                        store_tui::Focus::Readme => state.scroll_readme_down(20),
                    }
                }
                crossterm::event::KeyCode::Char('b')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    match state.focus {
                        store_tui::Focus::List => state.move_up(20),
                        store_tui::Focus::Readme => state.scroll_readme_up(20),
                    }
                }

                // ── Actions ──
                crossterm::event::KeyCode::Char('s') => {
                    state.sort_mode = state.sort_mode.next();
                    state.sort_plugins();
                    state.message = Some(format!("Sort: {}", state.sort_mode.label()));
                }
                crossterm::event::KeyCode::Char('R') => {
                    crate::store::clear_search_cache(&cache_root);
                    state.message = Some("Cache cleared. Searching...".to_string());
                    terminal.draw(|f| state.draw(f))?;
                    let cache_root_bg = cache_root.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        crate::store::fetch_popular(&cache_root_bg)
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
                        let result =
                            run_add(url.clone(), None, None, None, None, None, None, None).await;
                        let added = result.is_ok();
                        match result {
                            Ok(_) => println!("Added {} successfully!", url),
                            Err(e) => eprintln!("Failed to add {}: {}", url, e),
                        }

                        // TUI に戻る
                        print!("\nPress any key to return to store...");
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, MapSpec, Options, Plugin};
    use crate::loader::PluginScripts;
    use tempfile::tempdir;
    use toml_edit::DocumentMut;

    #[test]
    fn test_installed_full_name_owner_repo() {
        assert_eq!(
            installed_full_name("folke/snacks.nvim"),
            Some("folke/snacks.nvim".to_string())
        );
    }

    #[test]
    fn test_installed_full_name_https_url_with_git_suffix() {
        assert_eq!(
            installed_full_name("https://github.com/Owner/Repo.git"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_installed_full_name_https_url_without_git_suffix() {
        assert_eq!(
            installed_full_name("https://github.com/nvim-lua/plenary.nvim"),
            Some("nvim-lua/plenary.nvim".to_string())
        );
    }

    #[test]
    fn test_installed_full_name_ssh_url() {
        assert_eq!(
            installed_full_name("git@github.com:Owner/Repo.git"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_installed_full_name_non_github_returns_none() {
        assert_eq!(installed_full_name("https://gitlab.com/owner/repo"), None);
    }

    #[test]
    fn test_installed_full_name_case_normalized() {
        assert_eq!(
            installed_full_name("Folke/Snacks.NVIM"),
            Some("folke/snacks.nvim".to_string())
        );
    }

    #[test]
    fn test_installed_full_name_trailing_slash() {
        // `owner/repo/`, `.../repo.git/`, `.../repo/` をすべて許容する
        assert_eq!(
            installed_full_name("folke/snacks.nvim/"),
            Some("folke/snacks.nvim".to_string())
        );
        assert_eq!(
            installed_full_name("https://github.com/Owner/Repo/"),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            installed_full_name("https://github.com/Owner/Repo.git/"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_update_filters_by_query() {
        let plugins = [
            Plugin {
                url: "owner/telescope.nvim".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "owner/plenary.nvim".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "owner/nvim-cmp".to_string(),
                ..Default::default()
            },
        ];
        let query = Some("telescope".to_string());
        let filtered: Vec<_> = plugins
            .iter()
            .filter(|p| {
                if let Some(q) = &query {
                    p.url.contains(q.as_str())
                } else {
                    true
                }
            })
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].url, "owner/telescope.nvim");
    }

    #[test]
    fn test_update_no_query_matches_all() {
        let plugins = [
            Plugin {
                url: "owner/telescope.nvim".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "owner/plenary.nvim".to_string(),
                ..Default::default()
            },
        ];
        let query: Option<String> = None;
        let filtered: Vec<_> = plugins
            .iter()
            .filter(|p| {
                if let Some(q) = &query {
                    p.url.contains(q.as_str())
                } else {
                    true
                }
            })
            .collect();
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_expand_tilde_bare_tilde_returns_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~"), home);
    }

    #[test]
    fn test_expand_tilde_with_forward_slash_subpath() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/foo/bar"), home.join("foo").join("bar"));
    }

    #[test]
    fn test_expand_tilde_with_backslash_subpath() {
        let home = dirs::home_dir().unwrap();
        // Windows 入力形式にも対応
        let got = expand_tilde("~\\foo\\bar");
        // 実際のパス区切りは OS 依存だが、home 配下に foo と bar を含むかで判定
        let s = got.to_string_lossy().replace('\\', "/");
        let expected = home
            .join("foo")
            .join("bar")
            .to_string_lossy()
            .replace('\\', "/");
        assert_eq!(s, expected);
    }

    #[test]
    fn test_expand_tilde_absolute_path_untouched() {
        assert_eq!(
            expand_tilde("/absolute/path"),
            PathBuf::from("/absolute/path")
        );
    }

    #[test]
    fn test_expand_tilde_relative_path_untouched() {
        assert_eq!(
            expand_tilde("relative/path"),
            PathBuf::from("relative/path")
        );
    }

    #[test]
    fn test_is_valid_appname_rejects_unsafe_values() {
        assert!(is_valid_appname("nvim"));
        assert!(is_valid_appname("nvim-test"));
        assert!(!is_valid_appname(""));
        assert!(!is_valid_appname("."));
        assert!(!is_valid_appname(".."));
        assert!(!is_valid_appname("foo/bar"));
        assert!(!is_valid_appname("foo\\bar"));
        assert!(!is_valid_appname("foo\0bar"));
    }

    #[test]
    fn test_resolve_cache_root_uses_appname_default() {
        let home = dirs::home_dir().unwrap();
        let result = resolve_cache_root(None);
        // appname は env に依存するので親ディレクトリだけ確認
        assert!(result.starts_with(home.join(".cache").join("rvpm")));
    }

    #[test]
    fn test_resolve_cache_root_expands_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            resolve_cache_root(Some("~/dotfiles/rvpm")),
            home.join("dotfiles").join("rvpm")
        );
    }

    #[test]
    fn test_resolve_cache_root_accepts_absolute_path() {
        assert_eq!(
            resolve_cache_root(Some("/opt/rvpm")),
            PathBuf::from("/opt/rvpm")
        );
    }

    #[test]
    fn test_resolve_config_root_uses_default_when_none() {
        let home = dirs::home_dir().unwrap();
        let result = resolve_config_root(None);
        // デフォルトは `~/.config/rvpm/<appname>` (plugins は含まない)
        assert!(result.starts_with(home.join(".config").join("rvpm")));
        assert!(!result.ends_with("plugins"));
        // 末尾は appname (env 依存)
        assert_eq!(
            result.parent(),
            Some(home.join(".config").join("rvpm").as_path())
        );
    }

    #[test]
    fn test_resolve_config_root_expands_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            resolve_config_root(Some("~/dotfiles/nvim")),
            home.join("dotfiles").join("nvim")
        );
    }

    #[test]
    fn test_resolve_config_root_accepts_absolute_path() {
        assert_eq!(
            resolve_config_root(Some("/etc/rvpm")),
            PathBuf::from("/etc/rvpm")
        );
    }

    #[test]
    fn test_resolve_plugin_config_dir_joins_plugins_subdir() {
        let plugin = config::Plugin {
            url: "folke/snacks.nvim".to_string(),
            ..Default::default()
        };
        let root = PathBuf::from("/tmp/rvpm");
        let got = resolve_plugin_config_dir(&root, &plugin);
        assert_eq!(
            got,
            PathBuf::from("/tmp/rvpm")
                .join("plugins")
                .join(plugin.canonical_path())
        );
    }

    // -----------------------------------------------------------------
    // rvpm init ヘルパーのテスト
    // -----------------------------------------------------------------

    #[test]
    fn test_nvim_init_lua_path_for_appname_defaults_to_nvim() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            nvim_init_lua_path_for_appname(None),
            home.join(".config").join("nvim").join("init.lua")
        );
    }

    #[test]
    fn test_nvim_init_lua_path_for_appname_respects_nvim_appname() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            nvim_init_lua_path_for_appname(Some("mynvim")),
            home.join(".config").join("mynvim").join("init.lua")
        );
    }

    #[test]
    fn test_loader_init_snippet_uses_default_when_no_options() {
        let cfg = config::Config {
            vars: None,
            options: config::Options::default(),
            plugins: vec![],
        };
        let snippet = loader_init_snippet(&cfg);
        // appname は env 依存なので partial match
        assert!(snippet.starts_with("dofile(vim.fn.expand(\"~/.cache/rvpm/"));
        assert!(snippet.ends_with("/plugins/loader.lua\"))"));
    }

    #[test]
    fn test_loader_init_snippet_uses_cache_root_when_set() {
        let cfg = config::Config {
            vars: None,
            options: config::Options {
                cache_root: Some("~/dotfiles/rvpm".to_string()),
                ..Default::default()
            },
            plugins: vec![],
        };
        assert_eq!(
            loader_init_snippet(&cfg),
            "dofile(vim.fn.expand(\"~/dotfiles/rvpm/plugins/loader.lua\"))"
        );
    }

    #[test]
    fn test_loader_init_snippet_normalizes_windows_path_separators() {
        let cfg = config::Config {
            vars: None,
            options: config::Options {
                cache_root: Some(r"C:\Users\test\.cache\rvpm\nvim".to_string()),
                ..Default::default()
            },
            plugins: vec![],
        };
        let snippet = loader_init_snippet(&cfg);
        assert!(
            !snippet.contains('\\'),
            "snippet contains backslash: {snippet}"
        );
        assert_eq!(
            snippet,
            "dofile(vim.fn.expand(\"C:/Users/test/.cache/rvpm/nvim/plugins/loader.lua\"))"
        );
    }

    #[test]
    fn test_loader_init_snippet_trims_trailing_backslash() {
        let cfg = config::Config {
            vars: None,
            options: config::Options {
                cache_root: Some(r"C:\cache\rvpm\".to_string()),
                ..Default::default()
            },
            plugins: vec![],
        };
        assert_eq!(
            loader_init_snippet(&cfg),
            "dofile(vim.fn.expand(\"C:/cache/rvpm/plugins/loader.lua\"))"
        );
    }

    #[test]
    fn test_init_lua_references_rvpm_loader_detects_line() {
        let root = tempdir().unwrap();
        let path = root.path().join("init.lua");
        std::fs::write(
            &path,
            "-- some\ndofile(vim.fn.expand(\"~/.cache/rvpm/loader.lua\"))\n",
        )
        .unwrap();
        assert!(init_lua_references_rvpm_loader(&path));
    }

    #[test]
    fn test_init_lua_references_rvpm_loader_false_when_absent() {
        let root = tempdir().unwrap();
        let path = root.path().join("init.lua");
        std::fs::write(&path, "-- empty\nvim.g.mapleader = ' '\n").unwrap();
        assert!(!init_lua_references_rvpm_loader(&path));
    }

    #[test]
    fn test_init_lua_references_rvpm_loader_false_when_file_missing() {
        let root = tempdir().unwrap();
        let path = root.path().join("missing.lua");
        assert!(!init_lua_references_rvpm_loader(&path));
    }

    #[test]
    fn test_init_lua_references_rvpm_loader_requires_both_keywords() {
        let root = tempdir().unwrap();
        let path = root.path().join("init.lua");
        // "loader.lua" だけでは rvpm の loader 参照と判定しない
        std::fs::write(&path, "dofile(\"~/other/loader.lua\")\n").unwrap();
        assert!(!init_lua_references_rvpm_loader(&path));
    }

    #[test]
    fn test_write_init_lua_snippet_creates_when_missing() {
        let root = tempdir().unwrap();
        let init_path = root.path().join("nvim").join("init.lua");
        let snippet = "dofile(vim.fn.expand(\"~/.cache/rvpm/loader.lua\"))";
        let result = write_init_lua_snippet(&init_path, snippet).unwrap();
        assert!(matches!(result, WriteInitResult::Created));
        assert!(init_path.exists());
        let content = std::fs::read_to_string(&init_path).unwrap();
        assert!(content.contains(snippet));
        assert!(content.contains("rvpm"));
    }

    #[test]
    fn test_write_init_lua_snippet_appends_when_exists_without_loader() {
        let root = tempdir().unwrap();
        let init_path = root.path().join("init.lua");
        std::fs::write(&init_path, "-- existing\nvim.g.mapleader = ' '\n").unwrap();
        let snippet = "dofile(vim.fn.expand(\"~/.cache/rvpm/loader.lua\"))";
        let result = write_init_lua_snippet(&init_path, snippet).unwrap();
        assert!(matches!(result, WriteInitResult::Appended));
        let content = std::fs::read_to_string(&init_path).unwrap();
        assert!(content.contains("mapleader"));
        assert!(content.contains(snippet));
    }

    #[test]
    fn test_write_init_lua_snippet_noop_when_already_configured() {
        let root = tempdir().unwrap();
        let init_path = root.path().join("init.lua");
        std::fs::write(
            &init_path,
            "dofile(vim.fn.expand(\"~/.cache/rvpm/loader.lua\"))\n",
        )
        .unwrap();
        let result = write_init_lua_snippet(
            &init_path,
            "dofile(vim.fn.expand(\"~/.cache/rvpm/loader.lua\"))",
        )
        .unwrap();
        assert!(matches!(result, WriteInitResult::AlreadyConfigured));
        let content = std::fs::read_to_string(&init_path).unwrap();
        // 行数が増えていないこと
        assert_eq!(content.lines().count(), 1);
    }

    #[test]
    fn test_resolve_loader_path_is_under_plugins() {
        let base = PathBuf::from("/cache/rvpm/nvim");
        let result = resolve_loader_path(&base);
        assert_eq!(result, PathBuf::from("/cache/rvpm/nvim/plugins/loader.lua"));
    }

    #[test]
    fn test_write_loader_to_path_creates_file() {
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        std::fs::create_dir_all(&merged).unwrap();
        let loader_path = root.path().join("custom").join("loader.lua");
        let scripts: Vec<PluginScripts> = vec![];
        write_loader_to_path(
            &merged,
            &scripts,
            &loader_path,
            &crate::loader::LoaderOptions::default(),
        )
        .unwrap();
        assert!(loader_path.exists());
        let content = std::fs::read_to_string(&loader_path).unwrap();
        assert!(content.contains("-- rvpm generated loader.lua"));
    }

    #[test]
    fn test_resolve_concurrency_defaults_to_8() {
        let result = resolve_concurrency(None);
        assert_eq!(result, DEFAULT_CONCURRENCY);
        assert_eq!(result, 8);
    }

    #[test]
    fn test_resolve_concurrency_uses_config_value() {
        let result = resolve_concurrency(Some(5));
        assert_eq!(result, 5);
    }

    #[test]
    fn test_remove_from_toml() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n\n[[plugins]]\nurl = \"owner/b\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        remove_plugin_from_toml(&mut doc, "owner/a").unwrap();
        let result = doc.to_string();
        assert!(!result.contains("owner/a"));
        assert!(result.contains("owner/b"));
    }

    #[test]
    fn test_find_plugin_line_in_toml_basic() {
        let toml = "[options]\n\n[[plugins]]\nurl = \"owner/a\"\nlazy = true\n\n[[plugins]]\nurl = \"owner/b\"\n";
        //            1         2  3             4             5           6  7             8
        assert_eq!(find_plugin_line_in_toml(toml, "owner/a"), 4);
        assert_eq!(find_plugin_line_in_toml(toml, "owner/b"), 8);
    }

    #[test]
    fn test_find_plugin_line_in_toml_handles_whitespace_variants() {
        let toml = "[[plugins]]\nurl=\"owner/a\"\n\n[[plugins]]\nurl  =   \"owner/b\"\n";
        assert_eq!(find_plugin_line_in_toml(toml, "owner/a"), 2);
        assert_eq!(find_plugin_line_in_toml(toml, "owner/b"), 5);
    }

    #[test]
    fn test_find_plugin_line_in_toml_missing_falls_back_to_one() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        assert_eq!(find_plugin_line_in_toml(toml, "owner/nonexistent"), 1);
    }

    #[test]
    fn test_find_plugin_line_in_toml_ignores_substring_matches() {
        // "owner/ab" should not be matched when searching for "owner/a"
        let toml = "[[plugins]]\nurl = \"owner/ab\"\n\n[[plugins]]\nurl = \"owner/a\"\n";
        assert_eq!(find_plugin_line_in_toml(toml, "owner/a"), 5);
    }

    #[test]
    fn test_editor_supports_line_jump() {
        assert!(editor_supports_line_jump("nvim"));
        assert!(editor_supports_line_jump("vim"));
        assert!(editor_supports_line_jump("vi"));
        assert!(editor_supports_line_jump("nano"));
        assert!(editor_supports_line_jump("emacs"));
        assert!(editor_supports_line_jump("/usr/local/bin/nvim"));
        assert!(editor_supports_line_jump(
            "C:\\Program Files\\Neovim\\bin\\nvim.exe"
        ));
        assert!(!editor_supports_line_jump("code"));
        assert!(!editor_supports_line_jump("hx"));
    }

    #[test]
    fn test_remove_from_toml_not_found_returns_error() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        assert!(remove_plugin_from_toml(&mut doc, "owner/nonexistent").is_err());
    }

    #[test]
    fn test_set_plugin_list_field_single_writes_as_string() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        set_plugin_list_field(&mut doc, "owner/a", "on_cmd", vec!["Telescope".to_string()])
            .unwrap();
        let result = doc.to_string();
        assert!(
            result.contains("on_cmd = \"Telescope\""),
            "1要素は文字列として書かれるべき: {}",
            result
        );
        assert!(
            !result.contains("on_cmd = ["),
            "1要素は配列にしないべき: {}",
            result
        );
    }

    // -----------------------------------------------------------------
    // --on-* CLI パーサ (Vec<String> 用) のテスト
    // -----------------------------------------------------------------

    #[test]
    fn test_parse_cli_string_list_single_value() {
        let items = parse_cli_string_list("BufReadPre").unwrap();
        assert_eq!(items, vec!["BufReadPre".to_string()]);
    }

    #[test]
    fn test_parse_cli_string_list_comma_separated() {
        let items = parse_cli_string_list("BufReadPre, BufNewFile ,InsertEnter").unwrap();
        assert_eq!(
            items,
            vec![
                "BufReadPre".to_string(),
                "BufNewFile".to_string(),
                "InsertEnter".to_string(),
            ]
        );
    }

    #[test]
    fn test_parse_cli_string_list_json_array() {
        let items = parse_cli_string_list(r#"["BufReadPre", "BufNewFile"]"#).unwrap();
        assert_eq!(
            items,
            vec!["BufReadPre".to_string(), "BufNewFile".to_string()]
        );
    }

    #[test]
    fn test_parse_cli_string_list_json_array_with_user_event() {
        let items = parse_cli_string_list(r#"["BufReadPre", "User LazyVimStarted"]"#).unwrap();
        assert_eq!(items[1], "User LazyVimStarted");
    }

    #[test]
    fn test_parse_cli_string_list_malformed_json_errors() {
        // "[" で始まっていると JSON として扱うので、壊れた JSON はエラー
        let err = parse_cli_string_list(r#"[BufReadPre, BufNewFile]"#).unwrap_err();
        assert!(err.to_string().contains("JSON"));
    }

    #[test]
    fn test_parse_cli_string_list_trims_and_ignores_empty() {
        let items = parse_cli_string_list("  a  ,  ,b,").unwrap();
        assert_eq!(items, vec!["a".to_string(), "b".to_string()]);
    }

    // -----------------------------------------------------------------
    // --on-map CLI パーサ / writer のテスト
    // -----------------------------------------------------------------

    #[test]
    fn test_parse_on_map_cli_simple_single_string() {
        let specs = parse_on_map_cli("<leader>f").unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].lhs, "<leader>f");
        assert!(specs[0].mode.is_empty());
        assert_eq!(specs[0].desc, None);
    }

    #[test]
    fn test_parse_on_map_cli_comma_separated() {
        let specs = parse_on_map_cli("<leader>f, <leader>g ,<leader>h").unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].lhs, "<leader>f");
        assert_eq!(specs[1].lhs, "<leader>g");
        assert_eq!(specs[2].lhs, "<leader>h");
    }

    #[test]
    fn test_parse_on_map_cli_json_array_of_strings() {
        let specs = parse_on_map_cli(r#"["<leader>f", "<leader>g"]"#).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].lhs, "<leader>f");
        assert_eq!(specs[1].lhs, "<leader>g");
    }

    #[test]
    fn test_parse_on_map_cli_json_single_object() {
        let specs =
            parse_on_map_cli(r#"{ "lhs": "<space>d", "mode": ["n", "x"], "desc": "Delete" }"#)
                .unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].lhs, "<space>d");
        assert_eq!(specs[0].mode, vec!["n".to_string(), "x".to_string()]);
        assert_eq!(specs[0].desc.as_deref(), Some("Delete"));
    }

    #[test]
    fn test_parse_on_map_cli_json_object_mode_as_string() {
        let specs = parse_on_map_cli(r#"{ "lhs": "<leader>v", "mode": "v" }"#).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].lhs, "<leader>v");
        assert_eq!(specs[0].mode, vec!["v".to_string()]);
    }

    #[test]
    fn test_parse_on_map_cli_json_array_mixed() {
        let specs = parse_on_map_cli(
            r#"[
                "<leader>a",
                { "lhs": "<leader>b", "mode": "x" },
                { "lhs": "<leader>c", "mode": ["n", "v"], "desc": "C" }
            ]"#,
        )
        .unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].lhs, "<leader>a");
        assert!(specs[0].mode.is_empty());
        assert_eq!(specs[1].lhs, "<leader>b");
        assert_eq!(specs[1].mode, vec!["x".to_string()]);
        assert_eq!(specs[2].lhs, "<leader>c");
        assert_eq!(specs[2].mode, vec!["n".to_string(), "v".to_string()]);
        assert_eq!(specs[2].desc.as_deref(), Some("C"));
    }

    #[test]
    fn test_parse_on_map_cli_json_object_missing_lhs_errors() {
        let err = parse_on_map_cli(r#"{ "mode": ["n"] }"#).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("lhs"));
    }

    #[test]
    fn test_set_plugin_map_field_single_simple_writes_string() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        let specs = vec![MapSpec {
            lhs: "<leader>f".to_string(),
            mode: Vec::new(),
            desc: None,
        }];
        set_plugin_map_field(&mut doc, "owner/a", specs).unwrap();
        let result = doc.to_string();
        assert!(
            result.contains("on_map = \"<leader>f\""),
            "simple single spec should write as plain string: {}",
            result
        );
    }

    #[test]
    fn test_set_plugin_map_field_with_mode_writes_inline_table() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        let specs = vec![MapSpec {
            lhs: "<space>d".to_string(),
            mode: vec!["n".to_string(), "x".to_string()],
            desc: Some("Delete".to_string()),
        }];
        set_plugin_map_field(&mut doc, "owner/a", specs).unwrap();
        let result = doc.to_string();
        assert!(
            result.contains("lhs = \"<space>d\""),
            "should include lhs field: {}",
            result
        );
        assert!(
            result.contains("mode = [\"n\", \"x\"]") || result.contains("mode = [ \"n\", \"x\" ]"),
            "should include mode array: {}",
            result
        );
        assert!(
            result.contains("desc = \"Delete\""),
            "should include desc: {}",
            result
        );
    }

    #[test]
    fn test_set_plugin_map_field_mixed_writes_array_of_mixed() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        let specs = vec![
            MapSpec {
                lhs: "<leader>a".to_string(),
                mode: Vec::new(),
                desc: None,
            },
            MapSpec {
                lhs: "<leader>b".to_string(),
                mode: vec!["n".to_string(), "x".to_string()],
                desc: Some("B".to_string()),
            },
        ];
        set_plugin_map_field(&mut doc, "owner/a", specs).unwrap();
        let result = doc.to_string();
        // 配列 literal 内に単純文字列とインラインテーブルが混在
        assert!(
            result.contains("\"<leader>a\""),
            "simple item as string: {}",
            result
        );
        assert!(
            result.contains("lhs = \"<leader>b\""),
            "full item as inline table: {}",
            result
        );
        assert!(result.contains("desc = \"B\""));
    }

    #[test]
    fn test_set_plugin_list_field_multiple_writes_as_array() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        set_plugin_list_field(
            &mut doc,
            "owner/a",
            "on_event",
            vec!["BufRead".to_string(), "BufNewFile".to_string()],
        )
        .unwrap();
        let result = doc.to_string();
        assert!(
            result.contains("on_event = ["),
            "複数要素は配列として書かれるべき: {}",
            result
        );
        assert!(result.contains("\"BufRead\""));
        assert!(result.contains("\"BufNewFile\""));
    }

    #[test]
    fn test_update_plugin_config() {
        let toml = r#"[[plugins]]
url = "test/plugin"
lazy = false"#;
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        update_plugin_config(
            &mut doc,
            "test/plugin",
            Some(true),
            Some(true),
            None,
            None,
            Some("v1.0".to_string()),
        )
        .unwrap();
        let result = doc.to_string();
        assert!(result.contains("lazy = true"));
        assert!(result.contains("merge = true"));
        assert!(result.contains("rev = \"v1.0\""));
    }

    // -----------------------------------------------------------------
    // build コマンドのテスト
    // -----------------------------------------------------------------

    #[test]
    fn test_parse_build_command_shell() {
        let dirs = vec![PathBuf::from("/path/to/plugin")];
        let (cmd, args) = parse_build_command("cargo build --release", &dirs);
        if cfg!(windows) {
            assert_eq!(cmd, "cmd");
            assert_eq!(args, vec!["/C", "cargo build --release"]);
        } else {
            assert_eq!(cmd, "sh");
            assert_eq!(args, vec!["-c", "cargo build --release"]);
        }
    }

    #[test]
    fn test_parse_build_command_vim_prefix() {
        let dirs = vec![PathBuf::from("/path/to/plugin")];
        let (cmd, args) = parse_build_command(":call mkdp#util#install()", &dirs);
        assert_eq!(cmd, "nvim");
        assert!(args.iter().any(|a| a == "--headless"));
        assert!(args.iter().any(|a| a.contains("mkdp#util#install()")));
    }

    #[test]
    fn test_parse_build_command_vim_simple() {
        let dirs = vec![PathBuf::from("/path/to/plugin")];
        let (cmd, args) = parse_build_command(":TSUpdate", &dirs);
        assert_eq!(cmd, "nvim");
        assert!(args.iter().any(|a| a == "--headless"));
        assert!(args.iter().any(|a| a.contains("TSUpdate")));
    }

    #[test]
    fn test_parse_build_command_vim_adds_rtp() {
        let dirs = vec![PathBuf::from("/path/to/my-plugin")];
        let (cmd, args) = parse_build_command(":MyBuild", &dirs);
        assert_eq!(cmd, "nvim");
        assert!(args.iter().any(|a| a == "--cmd"));
        assert!(
            args.iter()
                .any(|a| a.contains("set rtp+=/path/to/my-plugin")),
            "should add plugin dir to rtp: {:?}",
            args
        );
    }

    #[test]
    fn test_parse_build_command_vim_includes_deps_rtp() {
        let dirs = vec![
            PathBuf::from("/path/to/plugin"),
            PathBuf::from("/path/to/dep1"),
            PathBuf::from("/path/to/dep2"),
        ];
        let (cmd, args) = parse_build_command(":Build", &dirs);
        assert_eq!(cmd, "nvim");
        let rtp_arg = args
            .iter()
            .find(|a| a.contains("set rtp+="))
            .expect("should have rtp cmd");
        assert!(rtp_arg.contains("/path/to/plugin"), "self: {}", rtp_arg);
        assert!(rtp_arg.contains("/path/to/dep1"), "dep1: {}", rtp_arg);
        assert!(rtp_arg.contains("/path/to/dep2"), "dep2: {}", rtp_arg);
    }

    #[test]
    fn test_find_unused_repos() {
        let root = tempdir().unwrap();
        let repos_dir = root.path().join("repos");
        std::fs::create_dir_all(&repos_dir).unwrap();
        let used_dir = repos_dir.join("github.com/used/plugin");
        let unused_dir = repos_dir.join("github.com/unused/plugin");
        std::fs::create_dir_all(used_dir.join(".git")).unwrap();
        std::fs::create_dir_all(unused_dir.join(".git")).unwrap();
        let config = Config {
            vars: None,
            options: Options::default(),
            plugins: vec![Plugin {
                url: "used/plugin".to_string(),
                ..Default::default()
            }],
        };
        let unused = find_unused_repos(&config, &repos_dir).unwrap();
        assert_eq!(unused.len(), 1);
        assert!(unused[0].to_string_lossy().contains("unused"));
    }

    #[test]
    fn test_prune_unused_repos_removes_listed_dirs() {
        let root = tempdir().unwrap();
        let a = root.path().join("a/.git");
        let b = root.path().join("b/.git");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let targets = vec![
            a.parent().unwrap().to_path_buf(),
            b.parent().unwrap().to_path_buf(),
        ];
        prune_unused_repos(&targets);
        assert!(!targets[0].exists());
        assert!(!targets[1].exists());
    }

    #[test]
    fn test_prune_unused_repos_empty_slice_noop() {
        // 空でもクラッシュしないこと
        prune_unused_repos(&[]);
    }

    #[test]
    fn test_plural_helper() {
        assert_eq!(plural("dir", "dirs", 0), "dirs");
        assert_eq!(plural("dir", "dirs", 1), "dir");
        assert_eq!(plural("dir", "dirs", 2), "dirs");
    }

    #[test]
    fn test_parse_config_auto_clean_defaults_to_false() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
"#;
        let config = crate::config::parse_config(toml).unwrap();
        assert!(!config.options.auto_clean);
    }

    #[test]
    fn test_parse_config_accepts_auto_clean_true() {
        let toml = r#"
[options]
auto_clean = true

[[plugins]]
url = "owner/repo"
"#;
        let config = crate::config::parse_config(toml).unwrap();
        assert!(config.options.auto_clean);
    }
}
