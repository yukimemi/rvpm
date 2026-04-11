mod config;
mod git;
mod link;
mod loader;
mod tui;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::task::JoinSet;
use crate::config::parse_config;
use crate::git::Repo;
use crate::link::merge_plugin;
use crate::loader::generate_loader;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Sync,
    Generate,
    Add {
        repo: String,
        #[arg(long)]
        name: Option<String>,
    },
    Edit {
        query: Option<String>,
    },
    Set {
        query: Option<String>,
        #[arg(long)]
        lazy: Option<bool>,
        #[arg(long)]
        merge: Option<bool>,
        #[arg(long)]
        on_cmd: Option<String>,
        #[arg(long)]
        on_ft: Option<String>,
        #[arg(long)]
        rev: Option<String>,
    },
    Update {
        query: Option<String>,
    },
    Remove {
        query: Option<String>,
    },
    Clean {
        #[arg(short, long)]
        force: bool,
    },
    Status,
    List,
    /// config.toml を $EDITOR で開く。編集後は sync を実行して変更を反映する。
    Config,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Sync => { run_sync().await?; },
        Commands::Generate => { run_generate().await?; },
        Commands::Add { repo, name } => { run_add(repo, name).await?; },
        Commands::Edit { query } => { if run_edit(query).await? { let _ = run_sync().await; } },
        Commands::Set { query, lazy, merge, on_cmd, on_ft, rev } => {
            if run_set(query, lazy, merge, on_cmd, on_ft, rev).await? { let _ = run_sync().await; }
        },
        Commands::Update { query } => { run_update(query).await?; },
        Commands::Remove { query } => { run_remove(query).await?; },
        Commands::Clean { force } => { run_clean(force).await?; },
        Commands::Status => { run_status().await?; },
        Commands::List => { run_list().await?; },
        Commands::Config => { if run_config().await? { let _ = run_sync().await; } },
    }

    Ok(())
}

use tokio::sync::mpsc;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use crate::tui::{TuiState, PluginStatus};

async fn run_sync() -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

    let mut config_data = parse_config(&toml_content)?;
    crate::config::sort_plugins(&mut config_data.plugins)?;
    let config = Arc::new(config_data);
    
    let base_dir = resolve_base_dir(config.options.base_dir.as_deref());
    let merged_dir = base_dir.join("merged");
    
    if merged_dir.exists() {
        let _ = std::fs::remove_dir_all(&merged_dir);
    }
    std::fs::create_dir_all(&merged_dir)?;

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
        let mut plugin = plugin.clone();
        let base_dir = base_dir.clone();
        let tx = tx.clone();
        let sem = semaphore.clone();
        if plugin.cond.is_some() { plugin.merge = false; }

        set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let dst_path = if let Some(d) = &plugin.dst { PathBuf::from(d) } else { base_dir.join("repos").join(plugin.canonical_path()) };
            let _ = tx.send((plugin.url.clone(), PluginStatus::Syncing("Syncing...".to_string()))).await;
            let repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
            let res = repo.sync().await;
            match res {
                Ok(_) => { let _ = tx.send((plugin.url.clone(), PluginStatus::Finished)).await; Ok((plugin, dst_path)) }
                Err(e) => { let _ = tx.send((plugin.url.clone(), PluginStatus::Failed(e.to_string()))).await; Err(e) }
            }
        });
    }

    let mut plugin_scripts = Vec::new();
    let mut finished_tasks = 0;
    let total_tasks = config.plugins.len();

    while finished_tasks < total_tasks {
        terminal.draw(|f| tui_state.draw(f))?;
        tokio::select! {
            Some((url, status)) = rx.recv() => { tui_state.update_status(&url, status); }
            Some(res) = set.join_next() => {
                finished_tasks += 1;
                if let Ok(Ok((plugin, dst_path))) = res {
                    if plugin.merge { let _ = merge_plugin(&dst_path, &merged_dir); }
                    let config_root = resolve_config_root(config.options.config_root.as_deref());
                    let plugin_config_dir = config_root.join(plugin.canonical_path());
                    let scripts = build_plugin_scripts(&plugin, &dst_path, &plugin_config_dir);
                    plugin_scripts.push(scripts);
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }
    terminal.draw(|f| tui_state.draw(f))?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    println!("Generating loader.lua...");
    let loader_path = resolve_loader_path(config.options.loader_path.as_deref(), &base_dir);
    write_loader_to_path(&merged_dir, &plugin_scripts, &loader_path)?;
    println!("Done! -> {}", loader_path.display());
    Ok(())
}

async fn run_generate() -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
    let mut config = parse_config(&toml_content)?;
    // depends に基づいた依存順に並べる (run_sync と同じ扱い)
    crate::config::sort_plugins(&mut config.plugins)?;
    let base_dir = resolve_base_dir(config.options.base_dir.as_deref());
    let merged_dir = base_dir.join("merged");
    let loader_path = resolve_loader_path(config.options.loader_path.as_deref(), &base_dir);

    let mut plugin_scripts = Vec::new();
    let config_root = resolve_config_root(config.options.config_root.as_deref());
    for plugin in &config.plugins {
        let dst_path = if let Some(d) = &plugin.dst {
            PathBuf::from(d)
        } else {
            base_dir.join("repos").join(plugin.canonical_path())
        };
        let plugin_config_dir = config_root.join(plugin.canonical_path());
        plugin_scripts.push(build_plugin_scripts(plugin, &dst_path, &plugin_config_dir));
    }

    println!("Generating loader.lua...");
    write_loader_to_path(&merged_dir, &plugin_scripts, &loader_path)?;
    println!("Done! -> {}", loader_path.display());
    Ok(())
}

/// 全プラグインの git 状態を並列で調べ、url -> PluginStatus のマップを返す。
async fn fetch_plugin_statuses(config: &config::Config, base_dir: &Path) -> std::collections::HashMap<String, PluginStatus> {
    let (tx, mut rx) = mpsc::channel::<(String, PluginStatus)>(100);
    let mut set = JoinSet::new();
    for plugin in config.plugins.iter() {
        let plugin = plugin.clone();
        let base_dir = base_dir.to_path_buf();
        let tx = tx.clone();
        set.spawn(async move {
            let dst_path = if let Some(d) = &plugin.dst {
                PathBuf::from(d)
            } else {
                base_dir.join("repos").join(plugin.canonical_path())
            };
            let repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
            let git_status = repo.get_status().await;
            let plugin_status = match git_status {
                crate::git::RepoStatus::Clean => PluginStatus::Finished,
                crate::git::RepoStatus::NotInstalled => PluginStatus::Failed("Missing".to_string()),
                crate::git::RepoStatus::Modified => PluginStatus::Syncing("Modified".to_string()),
                crate::git::RepoStatus::Outdated(msg) => PluginStatus::Syncing(format!("Outdated: {}", msg)),
                crate::git::RepoStatus::Error(e) => PluginStatus::Failed(e),
            };
            let _ = tx.send((plugin.url.clone(), plugin_status)).await;
        });
    }
    drop(tx);
    while set.join_next().await.is_some() {}
    let mut result = std::collections::HashMap::new();
    while let Ok((url, status)) = rx.try_recv() {
        result.insert(url, status);
    }
    result
}

async fn run_list() -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let mut config = parse_config(&toml_content)?;
    let base_dir = resolve_base_dir(config.options.base_dir.as_deref());

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
    let mut tui_state = TuiState::new(urls);

    // 初回のインストール状態チェック (進捗 TUI 付き)
    let (tx, mut rx) = mpsc::channel::<(String, PluginStatus)>(100);
    let mut set = JoinSet::new();
    for plugin in config.plugins.iter() {
        let plugin = plugin.clone();
        let base_dir = base_dir.clone();
        let tx = tx.clone();
        set.spawn(async move {
            let dst_path = if let Some(d) = &plugin.dst {
                PathBuf::from(d)
            } else {
                base_dir.join("repos").join(plugin.canonical_path())
            };
            let repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
            let git_status = repo.get_status().await;
            let plugin_status = match git_status {
                crate::git::RepoStatus::Clean => PluginStatus::Finished,
                crate::git::RepoStatus::NotInstalled => PluginStatus::Failed("Missing".to_string()),
                crate::git::RepoStatus::Modified => PluginStatus::Syncing("Modified".to_string()),
                crate::git::RepoStatus::Outdated(msg) => PluginStatus::Syncing(format!("Outdated: {}", msg)),
                crate::git::RepoStatus::Error(e) => PluginStatus::Failed(e),
            };
            let _ = tx.send((plugin.url.clone(), plugin_status)).await;
        });
    }
    drop(tx);

    let total = config.plugins.len();
    let mut done = 0;
    while done < total {
        terminal.draw(|f| tui_state.draw(f))?;
        tokio::select! {
            Some((url, status)) = rx.recv() => {
                tui_state.update_status(&url, status);
            }
            Some(_) = set.join_next() => { done += 1; }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }
    while let Ok((url, status)) = rx.try_recv() {
        tui_state.update_status(&url, status);
    }

    // アクション後に config とステータスを再読み込みしてTUIを復帰するヘルパー
    async fn reload_state(
        config_path: &Path,
        base_dir: &Path,
        terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<(config::Config, TuiState)> {
        let toml_content = std::fs::read_to_string(config_path)?;
        let config = parse_config(&toml_content)?;
        let statuses = fetch_plugin_statuses(&config, base_dir).await;
        let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
        let mut tui_state = TuiState::new(urls);
        for (url, status) in statuses {
            tui_state.update_status(&url, status);
        }
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen)?;
        terminal.clear()?;
        Ok((config, tui_state))
    }

    loop {
        terminal.draw(|f| tui_state.draw_list(f, &config))?;

        if crossterm::event::poll(std::time::Duration::from_millis(100))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.kind != crossterm::event::KeyEventKind::Press { continue; }

                match key.code {
                    crossterm::event::KeyCode::Char('q') => break,
                    crossterm::event::KeyCode::Char('j') | crossterm::event::KeyCode::Down => tui_state.next(),
                    crossterm::event::KeyCode::Char('k') | crossterm::event::KeyCode::Up => tui_state.previous(),
                    crossterm::event::KeyCode::Char('e') => {
                        if let Some(url) = tui_state.selected_url() {
                            disable_raw_mode()?;
                            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                            terminal.show_cursor()?;
                            if run_edit(Some(url)).await? { let _ = run_sync().await; }
                            let (c, s) = reload_state(&config_path, &base_dir, &mut terminal).await?;
                            config = c; tui_state = s;
                        }
                    }
                    crossterm::event::KeyCode::Char('s') => {
                        if let Some(url) = tui_state.selected_url() {
                            disable_raw_mode()?;
                            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                            terminal.show_cursor()?;
                            if run_set(Some(url), None, None, None, None, None).await? { let _ = run_sync().await; }
                            let (c, s) = reload_state(&config_path, &base_dir, &mut terminal).await?;
                            config = c; tui_state = s;
                        }
                    }
                    crossterm::event::KeyCode::Char('S') => {
                        disable_raw_mode()?;
                        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                        terminal.show_cursor()?;
                        let _ = run_sync().await;
                        let (c, s) = reload_state(&config_path, &base_dir, &mut terminal).await?;
                        config = c; tui_state = s;
                    }
                    crossterm::event::KeyCode::Char('u') => {
                        if let Some(url) = tui_state.selected_url() {
                            disable_raw_mode()?;
                            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                            terminal.show_cursor()?;
                            let _ = run_update(Some(url)).await;
                            let (c, s) = reload_state(&config_path, &base_dir, &mut terminal).await?;
                            config = c; tui_state = s;
                        }
                    }
                    crossterm::event::KeyCode::Char('U') => {
                        disable_raw_mode()?;
                        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                        terminal.show_cursor()?;
                        let _ = run_update(None).await;
                        let (c, s) = reload_state(&config_path, &base_dir, &mut terminal).await?;
                        config = c; tui_state = s;
                    }
                    crossterm::event::KeyCode::Char('g') => {
                        disable_raw_mode()?;
                        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                        terminal.show_cursor()?;
                        let _ = run_generate().await;
                        enable_raw_mode()?;
                        execute!(std::io::stdout(), EnterAlternateScreen)?;
                        terminal.clear()?;
                    }
                    crossterm::event::KeyCode::Char('d') => {
                        if let Some(url) = tui_state.selected_url() {
                            disable_raw_mode()?;
                            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                            terminal.show_cursor()?;
                            let _ = run_remove(Some(url)).await;
                            let (c, s) = reload_state(&config_path, &base_dir, &mut terminal).await?;
                            config = c; tui_state = s;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

async fn run_status() -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;
    let base_dir = resolve_base_dir(config.options.base_dir.as_deref());
    let mut set = JoinSet::new();
    println!("Checking plugin status...");
    for plugin in config.plugins.into_iter() {
        let base_dir = base_dir.clone();
        set.spawn(async move {
            let dst_path = if let Some(d) = &plugin.dst { PathBuf::from(d) } else { base_dir.join("repos").join(plugin.canonical_path()) };
            let repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
            let status = repo.get_status().await;
            (plugin.url.clone(), status)
        });
    }
    let mut results = Vec::new();
    while let Some(res) = set.join_next().await { results.push(res?); }
    results.sort_by(|a, b| a.0.cmp(&b.0));
    for (url, status) in results {
        match status {
            crate::git::RepoStatus::Clean => println!("  [Clean]     {}", url),
            crate::git::RepoStatus::NotInstalled => println!("  [Missing]   {}", url),
            crate::git::RepoStatus::Modified => println!("  [Modified]  {}", url),
            crate::git::RepoStatus::Outdated(msg) => println!("  [Outdated]  {} ({})", url, msg),
            crate::git::RepoStatus::Error(e) => println!("  [Error]     {} ({})", url, e),
        }
    }
    Ok(())
}

async fn run_update(query: Option<String>) -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
    let config_data = parse_config(&toml_content)?;
    let config = Arc::new(config_data);
    let base_dir = resolve_base_dir(config.options.base_dir.as_deref());

    let target_plugins: Vec<_> = config.plugins.iter()
        .filter(|p| {
            if let Some(q) = &query {
                p.url.contains(q.as_str())
                    || p.name.as_deref().map(|n| n.contains(q.as_str())).unwrap_or(false)
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
        let base_dir = base_dir.clone();
        let tx = tx.clone();
        let sem = semaphore.clone();

        set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let dst_path = if let Some(d) = &plugin.dst {
                PathBuf::from(d)
            } else {
                base_dir.join("repos").join(plugin.canonical_path())
            };
            let _ = tx.send((plugin.url.clone(), PluginStatus::Syncing("Updating...".to_string()))).await;
            let repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
            let res = repo.update().await;
            match res {
                Ok(_) => { let _ = tx.send((plugin.url.clone(), PluginStatus::Finished)).await; Ok(()) }
                Err(e) => { let _ = tx.send((plugin.url.clone(), PluginStatus::Failed(e.to_string()))).await; Err(e) }
            }
        });
    }

    let total_tasks = target_plugins.len();
    let mut finished_tasks = 0;

    while finished_tasks < total_tasks {
        terminal.draw(|f| tui_state.draw(f))?;
        tokio::select! {
            Some((url, status)) = rx.recv() => { tui_state.update_status(&url, status); }
            Some(_) = set.join_next() => { finished_tasks += 1; }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }
    terminal.draw(|f| tui_state.draw(f))?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    println!("Update complete. Regenerating loader.lua...");
    run_generate().await?;
    Ok(())
}

use toml_edit::{DocumentMut, table, value, Item};

async fn run_add(repo: String, name: Option<String>) -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let mut doc = toml_content.parse::<DocumentMut>()?;
    if doc.get("plugins").is_none() { doc["plugins"] = toml_edit::ArrayOfTables::new().into(); }
    let plugins = doc["plugins"].as_array_of_tables_mut().context("plugins is not an array of tables")?;
    for p in plugins.iter() { if p.get("url").and_then(|v| v.as_str()) == Some(&repo) { println!("Plugin already exists: {}", repo); return Ok(()); } }
    let mut new_plugin = table();
    new_plugin["url"] = value(&repo);
    if let Some(n) = name { new_plugin["name"] = value(n); }
    if let Item::Table(t) = new_plugin { plugins.push(t); }
    std::fs::write(&config_path, doc.to_string())?;
    println!("Added plugin to config: {}", repo);
    let _ = run_sync().await;
    Ok(())
}

use dialoguer::{FuzzySelect, Select};

/// `rvpm config` — config.toml を $EDITOR で直接開く。
/// ファイルが無ければ作らずにエラーを返す (init されていない場合のガード)。
/// 常に `Ok(true)` を返すので呼び出し側で sync を走らせる前提。
async fn run_config() -> Result<bool> {
    let config_path = rvpm_config_path();
    if !config_path.exists() {
        anyhow::bail!(
            "config file not found: {}\n\
             Create it first or run `rvpm add <repo>` to bootstrap.",
            config_path.display()
        );
    }
    println!("Opening {}", config_path.display());
    open_editor_at_line(&config_path, 1)?;
    Ok(true)
}

async fn run_edit(query: Option<String>) -> Result<bool> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    let plugin = if let Some(q) = query {
        config.plugins.iter().find(|p| p.url == q || p.url.contains(&q))
            .context("Plugin not found")?
    } else {
        let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
        let selection = FuzzySelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Select plugin to edit")
            .items(&urls)
            .interact_opt()?;
        match selection {
            Some(index) => config.plugins.iter().find(|p| p.url == urls[index]).unwrap(),
            None => return Ok(false),
        }
    };

    println!("\n>> Editing configuration for: {}", plugin.url);

    let files = vec!["init.lua", "before.lua", "after.lua"];
    let file_selection = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Select file to edit")
        .default(0)
        .items(&files)
        .interact_opt()?;

    let file_name = match file_selection {
        Some(index) => files[index],
        None => return Ok(false),
    };
    
    let config_root = resolve_config_root(config.options.config_root.as_deref());
    let plugin_config_dir = config_root.join(plugin.canonical_path());
    std::fs::create_dir_all(&plugin_config_dir)?;
    let target_file = plugin_config_dir.join(file_name);
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
    std::process::Command::new(editor).arg(target_file).status()?;
    Ok(true)
}

async fn run_set(query: Option<String>, lazy: Option<bool>, merge: Option<bool>, on_cmd: Option<String>, on_ft: Option<String>, rev: Option<String>) -> Result<bool> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    let selected_repo_url = if let Some(q) = query.as_ref() {
        config.plugins.iter().find(|p| &p.url == q || p.url.contains(q))
            .map(|p| p.url.clone())
            .context("Plugin not found")?
    } else {
        let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
        let selection = FuzzySelect::with_theme(&dialoguer::theme::ColorfulTheme::default()).with_prompt("Select plugin to set").items(&urls).interact_opt()?;
        match selection { Some(index) => urls[index].clone(), None => return Ok(false) }
    };

    println!("\n>> Setting options for: {}", selected_repo_url);
    let mut doc = toml_content.parse::<DocumentMut>()?;
    let mut modified = false;

    if lazy.is_some() || merge.is_some() || on_cmd.is_some() || on_ft.is_some() || rev.is_some() {
        let parse_list = |s: Option<String>| -> Option<Vec<String>> { s.map(|v| if v.trim().starts_with('[') { serde_json::from_str(&v).unwrap_or_else(|_| vec![v]) } else { v.split(',').map(|s| s.trim().to_string()).collect() }) };
        update_plugin_config(&mut doc, &selected_repo_url, lazy, merge, parse_list(on_cmd), parse_list(on_ft), rev)?;
        modified = true;
    } else {
        // 現在のプラグインを探して既存値をプレフィルに使う
        let current_plugin = config.plugins.iter().find(|p| p.url == selected_repo_url).cloned();
        let list_field_value = |field: &str| -> String {
            let Some(p) = current_plugin.as_ref() else { return String::new(); };
            // on_map は MapSpec の lhs だけを列挙する (mode/desc は手書き編集に委ねる)
            let items: Option<Vec<String>> = match field {
                "on_cmd" => p.on_cmd.clone(),
                "on_ft" => p.on_ft.clone(),
                "on_map" => p.on_map.as_ref().map(|v| v.iter().map(|m| m.lhs.clone()).collect()),
                "on_event" => p.on_event.clone(),
                "on_path" => p.on_path.clone(),
                "on_source" => p.on_source.clone(),
                _ => None,
            };
            items.map(|v| v.join(", ")).unwrap_or_default()
        };

        const EDITOR_SENTINEL: &str = "[ Open config.toml in $EDITOR ]";
        let options = vec![
            "lazy", "merge", "on_cmd", "on_ft", "on_map", "on_event", "on_path", "on_source", "rev",
            EDITOR_SENTINEL,
        ];
        let selection = Select::with_theme(&dialoguer::theme::ColorfulTheme::default()).with_prompt("Select option to set").items(&options).interact_opt()?;
        match selection {
            Some(index) => {
                match options[index] {
                    s if s == EDITOR_SENTINEL => {
                        // 対応 editor なら plugin の url 行にジャンプ
                        let line = find_plugin_line_in_toml(&toml_content, &selected_repo_url);
                        open_editor_at_line(&config_path, line)?;
                        // ユーザーが何を編集したか分からないので常に変更ありと見なす
                        return Ok(true);
                    }
                    "lazy" | "merge" => {
                        let current = current_plugin.as_ref().map(|p| {
                            if options[index] == "lazy" { p.lazy } else { p.merge }
                        }).unwrap_or(false);
                        let default_idx = if current { 0 } else { 1 };
                        let val = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                            .with_prompt(format!("Set {} to (current: {})", options[index], current))
                            .items(&["true", "false"])
                            .default(default_idx)
                            .interact_opt()?;
                        if let Some(v) = val {
                            update_plugin_config(&mut doc, &selected_repo_url, if options[index] == "lazy" { Some(v == 0) } else { None }, if options[index] == "merge" { Some(v == 0) } else { None }, None, None, None)?;
                            modified = true;
                        } else { return Ok(false); }
                    }
                    "on_map" => {
                        // on_map は table 形式 (mode/desc) もあるので edit mode を先に聞く
                        let modes = &["Edit lhs list only (CLI, mode/desc lost)", "Open config.toml in $EDITOR"];
                        let mode_sel = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
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
                                        let items: Vec<String> = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                                        set_plugin_list_field(&mut doc, &selected_repo_url, "on_map", items)?;
                                        modified = true;
                                    }
                                    _ => return Ok(false),
                                }
                            }
                            Some(1) => {
                                let line = find_plugin_line_in_toml(&toml_content, &selected_repo_url);
                                open_editor_at_line(&config_path, line)?;
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
                                let items: Vec<String> = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                                set_plugin_list_field(&mut doc, &selected_repo_url, field, items)?;
                                modified = true;
                            }
                            _ => return Ok(false),
                        }
                    }
                    "rev" => {
                        let existing = current_plugin.as_ref().and_then(|p| p.rev.clone()).unwrap_or_default();
                        let val = read_input_with_esc("Enter rev (branch/tag/hash, Esc to cancel)", &existing)?;
                        match val {
                            Some(v) if !v.is_empty() => {
                                update_plugin_config(&mut doc, &selected_repo_url, None, None, None, None, Some(v))?;
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
        std::fs::write(&config_path, doc.to_string())?;
        println!("Updated config for: {}", selected_repo_url);
        return Ok(true);
    }
    Ok(false)
}

async fn run_clean(force: bool) -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;
    let base_dir = resolve_base_dir(config.options.base_dir.as_deref());
    let repos_dir = base_dir.join("repos");
    let unused = find_unused_repos(&config, &repos_dir)?;
    if unused.is_empty() { println!("No unused plugins found."); return Ok(()); }
    println!("Found unused plugin directories:");
    for path in &unused { println!("  {}", path.display()); }
    let confirm = if force { true } else { dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default()).with_prompt("Do you want to delete these directories?").default(false).interact()? };
    if confirm { for path in unused { println!("Deleting {}...", path.display()); let _ = std::fs::remove_dir_all(path); } println!("Cleanup complete."); }
    Ok(())
}


fn find_unused_repos(config: &config::Config, repos_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut unused = Vec::new();
    let mut used_paths = std::collections::HashSet::new();
    for plugin in &config.plugins { used_paths.insert(repos_dir.join(plugin.canonical_path())); }
    for entry in walkdir::WalkDir::new(repos_dir).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_name() == ".git") {
        let git_dir = entry.path();
        if let Some(repo_root) = git_dir.parent() { if !used_paths.contains(repo_root) { unused.push(repo_root.to_path_buf()); } }
    }
    Ok(unused)
}

fn remove_plugin_from_toml(doc: &mut DocumentMut, url: &str) -> Result<()> {
    let plugins = doc["plugins"].as_array_of_tables_mut()
        .context("plugins is not an array of tables")?;
    let idx = plugins.iter().position(|p| {
        p.get("url").and_then(|v| v.as_str()) == Some(url)
    }).context("Plugin not found in config")?;
    plugins.remove(idx);
    Ok(())
}

async fn run_remove(query: Option<String>) -> Result<()> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    let selected_url = if let Some(q) = query.as_ref() {
        config.plugins.iter()
            .find(|p| p.url == *q || p.url.contains(q.as_str()))
            .map(|p| p.url.clone())
            .context("Plugin not found")?
    } else {
        let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
        let selection = FuzzySelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Select plugin to remove")
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
    std::fs::write(&config_path, doc.to_string())?;
    println!("Removed '{}' from config.", selected_url);

    let base_dir = resolve_base_dir(config.options.base_dir.as_deref());
    let plugin = config.plugins.iter().find(|p| p.url == selected_url).unwrap();
    let dst_path = if let Some(d) = &plugin.dst {
        PathBuf::from(d)
    } else {
        base_dir.join("repos").join(plugin.canonical_path())
    };

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
fn set_plugin_list_field(doc: &mut DocumentMut, url: &str, field: &str, values: Vec<String>) -> Result<()> {
    let plugins = doc["plugins"].as_array_of_tables_mut().context("plugins is not an array of tables")?;
    let plugin_table = plugins.iter_mut().find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url)).context("Could not find plugin in toml_edit document")?;
    if values.len() == 1 {
        plugin_table[field] = value(values.into_iter().next().unwrap());
    } else {
        let mut array = toml_edit::Array::new();
        for v in values { array.push(v); }
        plugin_table[field] = value(array);
    }
    Ok(())
}

fn update_plugin_config(doc: &mut DocumentMut, url: &str, lazy: Option<bool>, merge: Option<bool>, on_cmd: Option<Vec<String>>, on_ft: Option<Vec<String>>, rev: Option<String>) -> Result<()> {
    if let Some(l) = lazy {
        let plugins = doc["plugins"].as_array_of_tables_mut().context("plugins is not an array of tables")?;
        let plugin_table = plugins.iter_mut().find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url)).context("Could not find plugin in toml_edit document")?;
        plugin_table["lazy"] = value(l);
    }
    if let Some(m) = merge {
        let plugins = doc["plugins"].as_array_of_tables_mut().context("plugins is not an array of tables")?;
        let plugin_table = plugins.iter_mut().find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url)).context("Could not find plugin in toml_edit document")?;
        plugin_table["merge"] = value(m);
    }
    if let Some(cmds) = on_cmd { set_plugin_list_field(doc, url, "on_cmd", cmds)?; }
    if let Some(fts) = on_ft { set_plugin_list_field(doc, url, "on_ft", fts)?; }
    if let Some(r) = rev {
        let plugins = doc["plugins"].as_array_of_tables_mut().context("plugins is not an array of tables")?;
        let plugin_table = plugins.iter_mut().find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url)).context("Could not find plugin in toml_edit document")?;
        plugin_table["rev"] = value(r);
    }
    Ok(())
}

fn resolve_loader_path(config_loader_path: Option<&str>, base_dir: &Path) -> PathBuf {
    match config_loader_path {
        Some(raw) => expand_tilde(raw),
        None => base_dir.join("loader.lua"),
    }
}

fn write_loader_to_path(merged_dir: &Path, scripts: &[crate::loader::PluginScripts], loader_path: &Path) -> Result<()> {
    if let Some(parent) = loader_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lua = generate_loader(merged_dir, scripts);
    std::fs::write(loader_path, lua)?;
    Ok(())
}

fn resolve_concurrency(config_value: Option<usize>) -> usize {
    config_value.unwrap_or(tokio::sync::Semaphore::MAX_PERMITS)
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
//   - options.base_dir    → 全データの root (repos / merged / loader まとめて)
//   - options.loader_path → loader.lua のみ細かく上書き (base_dir より優先)
//   - options.config_root → per-plugin init/before/after.lua の置き場
//
// config.toml 自体の場所は固定 (~/.config/rvpm/config.toml)。これを読まないと
// options が取れないので chicken-and-egg を避けるため動かさない。
// ====================================================================

/// `~/.config/rvpm/config.toml` (固定)
fn rvpm_config_path() -> PathBuf {
    let home = dirs::home_dir().expect("Could not find home directory");
    home.join(".config").join("rvpm").join("config.toml")
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

/// rvpm のデータ置き場 root を決定する。
/// `options.base_dir` が設定されていればそれを tilde 展開して返す。
/// 未設定なら `~/.cache/rvpm` (デフォルト)。
fn resolve_base_dir(config_base_dir: Option<&str>) -> PathBuf {
    match config_base_dir {
        Some(raw) => expand_tilde(raw),
        None => {
            let home = dirs::home_dir().expect("Could not find home directory");
            home.join(".cache").join("rvpm")
        }
    }
}

/// per-plugin の init/before/after.lua を置く root を決定する。
/// `options.config_root` が設定されていればそれを tilde 展開して返す。
/// 未設定なら `~/.config/rvpm/plugins` (デフォルト)。
fn resolve_config_root(config_root: Option<&str>) -> PathBuf {
    match config_root {
        Some(raw) => expand_tilde(raw),
        None => {
            let home = dirs::home_dir().expect("Could not find home directory");
            home.join(".config").join("rvpm").join("plugins")
        }
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
    let base = std::path::Path::new(editor_cmd)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
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
    use crossterm::event::{Event, KeyCode, KeyModifiers, KeyEventKind};
    use std::io::Write;

    let mut input = String::from(initial);
    print!("{}: {}", prompt, input);
    std::io::stdout().flush()?;

    crossterm::terminal::enable_raw_mode()?;

    let result = loop {
        match crossterm::event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match key.code {
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
                    KeyCode::Backspace => {
                        if !input.is_empty() {
                            input.pop();
                            print!("\x08 \x08");
                            std::io::stdout().flush()?;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    };

    crossterm::terminal::disable_raw_mode()?;
    println!();
    result
}

fn find_lua(dir: &Path, name: &str) -> Option<String> {
    let path = dir.join(name);
    if path.exists() { Some(path.to_string_lossy().to_string()) } else { None }
}

/// 指定ディレクトリ配下を再帰的に walk し、`.vim` / `.lua` ファイルをソートして返す。
/// lazy.nvim の Util.walk + source_runtime のフィルタと同等。
/// ディレクトリが存在しない場合は空配列を返す (Resilience)。
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
        name: plugin.name.clone().unwrap_or_else(|| plugin.url.clone()),
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
        cond: plugin.cond.clone(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Plugin, Options};
    use crate::loader::PluginScripts;
    use tempfile::tempdir;
    use toml_edit::DocumentMut;

    #[test]
    fn test_update_filters_by_query() {
        let plugins = vec![
            Plugin { url: "owner/telescope.nvim".to_string(), ..Default::default() },
            Plugin { url: "owner/plenary.nvim".to_string(), ..Default::default() },
            Plugin { url: "owner/nvim-cmp".to_string(), ..Default::default() },
        ];
        let query = Some("telescope".to_string());
        let filtered: Vec<_> = plugins.iter()
            .filter(|p| {
                if let Some(q) = &query { p.url.contains(q.as_str()) } else { true }
            })
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].url, "owner/telescope.nvim");
    }

    #[test]
    fn test_update_no_query_matches_all() {
        let plugins = vec![
            Plugin { url: "owner/telescope.nvim".to_string(), ..Default::default() },
            Plugin { url: "owner/plenary.nvim".to_string(), ..Default::default() },
        ];
        let query: Option<String> = None;
        let filtered: Vec<_> = plugins.iter()
            .filter(|p| {
                if let Some(q) = &query { p.url.contains(q.as_str()) } else { true }
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
        let expected = home.join("foo").join("bar").to_string_lossy().replace('\\', "/");
        assert_eq!(s, expected);
    }

    #[test]
    fn test_expand_tilde_absolute_path_untouched() {
        assert_eq!(expand_tilde("/absolute/path"), PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_expand_tilde_relative_path_untouched() {
        assert_eq!(expand_tilde("relative/path"), PathBuf::from("relative/path"));
    }

    #[test]
    fn test_resolve_base_dir_uses_default_when_none() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(resolve_base_dir(None), home.join(".cache").join("rvpm"));
    }

    #[test]
    fn test_resolve_base_dir_expands_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            resolve_base_dir(Some("~/dotfiles/rvpm")),
            home.join("dotfiles").join("rvpm")
        );
    }

    #[test]
    fn test_resolve_base_dir_accepts_absolute_path() {
        assert_eq!(
            resolve_base_dir(Some("/opt/rvpm")),
            PathBuf::from("/opt/rvpm")
        );
    }

    #[test]
    fn test_resolve_config_root_uses_default_when_none() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            resolve_config_root(None),
            home.join(".config").join("rvpm").join("plugins")
        );
    }

    #[test]
    fn test_resolve_config_root_expands_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            resolve_config_root(Some("~/dotfiles/nvim/plugins")),
            home.join("dotfiles").join("nvim").join("plugins")
        );
    }

    #[test]
    fn test_resolve_config_root_accepts_absolute_path() {
        assert_eq!(
            resolve_config_root(Some("/etc/rvpm/plugins")),
            PathBuf::from("/etc/rvpm/plugins")
        );
    }

    #[test]
    fn test_resolve_loader_path_uses_default_when_none() {
        let base = PathBuf::from("/cache/rvpm");
        let result = resolve_loader_path(None, &base);
        assert_eq!(result, PathBuf::from("/cache/rvpm/loader.lua"));
    }

    #[test]
    fn test_resolve_loader_path_expands_tilde() {
        let base = PathBuf::from("/cache/rvpm");
        let result = resolve_loader_path(Some("~/.cache/nvim/loader.lua"), &base);
        assert!(result.to_str().unwrap().ends_with(".cache/nvim/loader.lua"));
        assert!(!result.to_str().unwrap().contains('~'));
    }

    #[test]
    fn test_resolve_loader_path_uses_absolute_path() {
        let base = PathBuf::from("/cache/rvpm");
        let result = resolve_loader_path(Some("/custom/path/loader.lua"), &base);
        assert_eq!(result, PathBuf::from("/custom/path/loader.lua"));
    }

    #[test]
    fn test_write_loader_to_path_creates_file() {
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        std::fs::create_dir_all(&merged).unwrap();
        let loader_path = root.path().join("custom").join("loader.lua");
        let scripts: Vec<PluginScripts> = vec![];
        write_loader_to_path(&merged, &scripts, &loader_path).unwrap();
        assert!(loader_path.exists());
        let content = std::fs::read_to_string(&loader_path).unwrap();
        assert!(content.contains("-- rvpm generated loader.lua"));
    }

    #[test]
    fn test_resolve_concurrency_defaults_to_max_permits() {
        let result = resolve_concurrency(None);
        assert_eq!(result, tokio::sync::Semaphore::MAX_PERMITS);
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
        assert!(editor_supports_line_jump("C:\\Program Files\\Neovim\\bin\\nvim.exe"));
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
        set_plugin_list_field(&mut doc, "owner/a", "on_cmd", vec!["Telescope".to_string()]).unwrap();
        let result = doc.to_string();
        assert!(result.contains("on_cmd = \"Telescope\""),
            "1要素は文字列として書かれるべき: {}", result);
        assert!(!result.contains("on_cmd = ["),
            "1要素は配列にしないべき: {}", result);
    }

    #[test]
    fn test_set_plugin_list_field_multiple_writes_as_array() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        set_plugin_list_field(&mut doc, "owner/a", "on_event", vec!["BufRead".to_string(), "BufNewFile".to_string()]).unwrap();
        let result = doc.to_string();
        assert!(result.contains("on_event = ["), "複数要素は配列として書かれるべき: {}", result);
        assert!(result.contains("\"BufRead\""));
        assert!(result.contains("\"BufNewFile\""));
    }

#[test]
    fn test_update_plugin_config() {
        let toml = r#"[[plugins]]
url = "test/plugin"
lazy = false"#;
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        update_plugin_config(&mut doc, "test/plugin", Some(true), Some(true), None, None, Some("v1.0".to_string())).unwrap();
        let result = doc.to_string();
        assert!(result.contains("lazy = true"));
        assert!(result.contains("merge = true"));
        assert!(result.contains("rev = \"v1.0\""));
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
            plugins: vec![Plugin { url: "used/plugin".to_string(), ..Default::default() }],
        };
        let unused = find_unused_repos(&config, &repos_dir).unwrap();
        assert_eq!(unused.len(), 1);
        assert!(unused[0].to_string_lossy().contains("unused"));
    }
}
