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
use crate::config::{parse_config, Plugin};
use crate::git::Repo;
use crate::link::merge_plugin;
use crate::loader::{generate_loader, PluginScripts};

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
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

    let mut config_data = parse_config(&toml_content)?;
    crate::config::sort_plugins(&mut config_data.plugins)?;
    let config = Arc::new(config_data);
    
    let base_dir = home.join(".cache/rvpm");
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
                    if let Some(config_root) = &config.options.config_root {
                        let plugin_config_dir = Path::new(config_root).join(plugin.canonical_path());
                        let scripts = PluginScripts {
                            name: plugin.name.clone().unwrap_or_else(|| plugin.url.clone()),
                            path: dst_path.to_string_lossy().to_string(),
                            init: find_lua(&plugin_config_dir, "init.lua"),
                            before: find_lua(&plugin_config_dir, "before.lua"),
                            after: find_lua(&plugin_config_dir, "after.lua"),
                            lazy: plugin.lazy,
                            on_cmd: plugin.on_cmd.clone(),
                            on_ft: plugin.on_ft.clone(),
                            on_map: plugin.on_map.clone(),
                            on_event: plugin.on_event.clone(),
                            on_path: plugin.on_path.clone(),
                            on_source: plugin.on_source.clone(),
                            cond: plugin.cond.clone(),
                        };
                        plugin_scripts.push(scripts);
                    }
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
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
    let config = parse_config(&toml_content)?;
    let base_dir = home.join(".cache/rvpm");
    let merged_dir = base_dir.join("merged");
    let loader_path = resolve_loader_path(config.options.loader_path.as_deref(), &base_dir);

    let mut plugin_scripts = Vec::new();
    if let Some(config_root) = &config.options.config_root {
        for plugin in &config.plugins {
            let dst_path = if let Some(d) = &plugin.dst {
                PathBuf::from(d)
            } else {
                base_dir.join("repos").join(plugin.canonical_path())
            };
            let plugin_config_dir = Path::new(config_root).join(plugin.canonical_path());
            plugin_scripts.push(PluginScripts {
                name: plugin.name.clone().unwrap_or_else(|| plugin.url.clone()),
                path: dst_path.to_string_lossy().to_string(),
                init: find_lua(&plugin_config_dir, "init.lua"),
                before: find_lua(&plugin_config_dir, "before.lua"),
                after: find_lua(&plugin_config_dir, "after.lua"),
                lazy: plugin.lazy,
                on_cmd: plugin.on_cmd.clone(),
                on_ft: plugin.on_ft.clone(),
                on_map: plugin.on_map.clone(),
                on_event: plugin.on_event.clone(),
                on_path: plugin.on_path.clone(),
                on_source: plugin.on_source.clone(),
                cond: plugin.cond.clone(),
            });
        }
    }

    println!("Generating loader.lua...");
    write_loader_to_path(&merged_dir, &plugin_scripts, &loader_path)?;
    println!("Done! -> {}", loader_path.display());
    Ok(())
}

async fn run_list() -> Result<()> {
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;
    
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
    let mut tui_state = TuiState::new(urls);

    // インストール状態の初期チェック
    let base_dir = home.join(".cache/rvpm");
    for plugin in &config.plugins {
        let dst_path = if let Some(d) = &plugin.dst {
            PathBuf::from(d)
        } else {
            base_dir.join("repos").join(plugin.canonical_path())
        };
        if dst_path.exists() {
            tui_state.update_status(&plugin.url, PluginStatus::Finished);
        } else {
            tui_state.update_status(&plugin.url, PluginStatus::Failed("Missing".to_string()));
        }
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
                            enable_raw_mode()?;
                            execute!(std::io::stdout(), EnterAlternateScreen)?;
                            terminal.clear()?;
                        }
                    }
                    crossterm::event::KeyCode::Char('s') => {
                        if let Some(url) = tui_state.selected_url() {
                            disable_raw_mode()?;
                            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                            terminal.show_cursor()?;
                            if run_set(Some(url), None, None, None, None, None).await? { let _ = run_sync().await; }
                            enable_raw_mode()?;
                            execute!(std::io::stdout(), EnterAlternateScreen)?;
                            terminal.clear()?;
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
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;
    let base_dir = home.join(".cache/rvpm");
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
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
    let config_data = parse_config(&toml_content)?;
    let config = Arc::new(config_data);
    let base_dir = home.join(".cache/rvpm");

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
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
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

async fn run_edit(query: Option<String>) -> Result<bool> {
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
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
    
    if let Some(config_root) = &config.options.config_root {
        let plugin_config_dir = Path::new(config_root).join(plugin.canonical_path());
        std::fs::create_dir_all(&plugin_config_dir)?;
        let target_file = plugin_config_dir.join(file_name);
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
        std::process::Command::new(editor).arg(target_file).status()?;
        return Ok(true);
    }
    Ok(false)
}

async fn run_set(query: Option<String>, lazy: Option<bool>, merge: Option<bool>, on_cmd: Option<String>, on_ft: Option<String>, rev: Option<String>) -> Result<bool> {
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
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
        let options = vec!["lazy", "merge", "on_cmd", "on_ft", "rev"];
        let selection = Select::with_theme(&dialoguer::theme::ColorfulTheme::default()).with_prompt("Select option to set").items(&options).interact_opt()?;
        match selection {
            Some(index) => {
                match options[index] {
                    "lazy" | "merge" => {
                        let val = Select::with_theme(&dialoguer::theme::ColorfulTheme::default()).with_prompt(format!("Set {} to", options[index])).items(&["true", "false"]).interact_opt()?;
                        if let Some(v) = val {
                            update_plugin_config(&mut doc, &selected_repo_url, if options[index] == "lazy" { Some(v == 0) } else { None }, if options[index] == "merge" { Some(v == 0) } else { None }, None, None, None)?;
                            modified = true;
                        } else { return Ok(false); }
                    }
                    "on_cmd" | "on_ft" => {
                        let input_res: Result<String, _> = dialoguer::Input::<String>::new().with_prompt(format!("Enter {} (comma separated)", options[index])).allow_empty(true).interact_text();
                        match input_res { Ok(val) if !val.is_empty() => {
                            let cmds: Vec<String> = val.split(',').map(|s| s.trim().to_string()).collect();
                            update_plugin_config(&mut doc, &selected_repo_url, None, None, if options[index] == "on_cmd" { Some(cmds.clone()) } else { None }, if options[index] == "on_ft" { Some(cmds) } else { None }, None)?;
                            modified = true;
                        } _ => return Ok(false) }
                    }
                    "rev" => {
                        let input_res: Result<String, _> = dialoguer::Input::<String>::new().with_prompt(format!("Enter {}", options[index])).allow_empty(true).interact_text();
                        match input_res { Ok(val) if !val.is_empty() => {
                            update_plugin_config(&mut doc, &selected_repo_url, None, None, None, None, Some(val))?;
                            modified = true;
                        } _ => return Ok(false) }
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
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;
    let base_dir = home.join(".cache/rvpm");
    let repos_dir = base_dir.join("repos");
    let unused = find_unused_repos(&config, &repos_dir)?;
    if unused.is_empty() { println!("No unused plugins found."); return Ok(()); }
    println!("Found unused plugin directories:");
    for path in &unused { println!("  {}", path.display()); }
    let confirm = if force { true } else { dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default()).with_prompt("Do you want to delete these directories?").default(false).interact()? };
    if confirm { for path in unused { println!("Deleting {}...", path.display()); let _ = std::fs::remove_dir_all(path); } println!("Cleanup complete."); }
    Ok(())
}

fn select_plugin_interactively(plugins: &[Plugin], query: Option<&str>) -> Result<String> {
    let urls: Vec<String> = plugins.iter().map(|p| p.url.clone()).collect();
    let selection = FuzzySelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Select plugin")
        .with_initial_text(query.unwrap_or(""))
        .items(&urls)
        .interact_opt()?;
    match selection {
        Some(index) => Ok(urls[index].clone()),
        None => anyhow::bail!("No plugin selected"),
    }
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
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
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

    let base_dir = home.join(".cache/rvpm");
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

fn update_plugin_config(doc: &mut DocumentMut, url: &str, lazy: Option<bool>, merge: Option<bool>, on_cmd: Option<Vec<String>>, on_ft: Option<Vec<String>>, rev: Option<String>) -> Result<()> {
    let plugins = doc["plugins"].as_array_of_tables_mut().context("plugins is not an array of tables")?;
    let plugin_table = plugins.iter_mut().find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url)).context("Could not find plugin in toml_edit document")?;
    if let Some(l) = lazy { plugin_table["lazy"] = value(l); }
    if let Some(m) = merge { plugin_table["merge"] = value(m); }
    if let Some(cmds) = on_cmd { let mut array = toml_edit::Array::new(); for cmd in cmds { array.push(cmd); } plugin_table["on_cmd"] = value(array); }
    if let Some(fts) = on_ft { let mut array = toml_edit::Array::new(); for ft in fts { array.push(ft); } plugin_table["on_ft"] = value(array); }
    if let Some(r) = rev { plugin_table["rev"] = value(r); }
    Ok(())
}

fn resolve_loader_path(config_loader_path: Option<&str>, base_dir: &Path) -> PathBuf {
    if let Some(raw) = config_loader_path {
        if raw.starts_with('~') {
            let home = dirs::home_dir().expect("Could not find home directory");
            home.join(&raw[2..])
        } else {
            PathBuf::from(raw)
        }
    } else {
        base_dir.join("loader.lua")
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

fn find_lua(dir: &Path, name: &str) -> Option<String> {
    let path = dir.join(name);
    if path.exists() { Some(path.to_string_lossy().to_string()) } else { None }
}

fn format_plugin_list(config: &config::Config) -> String {
    let mut out = String::new();
    out.push_str(&format!("{:<40} | {:<10} | {:<10} | {:<10}\n", "URL", "Status", "Merge", "Rev"));
    out.push_str(&format!("{:-<40}-+-{:-<10}-+-{:-<10}-+-{:-<10}\n", "", "", "", ""));
    for plugin in &config.plugins {
        let status = if plugin.lazy { "Lazy" } else { "Eager" };
        let merge = if plugin.merge { "Yes" } else { "No" };
        let rev = plugin.rev.as_deref().unwrap_or("-");
        out.push_str(&format!("{:<40} | {:<10} | {:<10} | {:<10}\n", plugin.url, status, merge, rev));
    }
    out
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
    fn test_remove_from_toml_not_found_returns_error() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        assert!(remove_plugin_from_toml(&mut doc, "owner/nonexistent").is_err());
    }

    #[test]
    fn test_format_plugin_list() {
        let config = Config {
            vars: None,
            options: Options::default(),
            plugins: vec![
                Plugin { url: "owner/repo1".to_string(), lazy: true, merge: false, rev: Some("v1.0".to_string()), ..Default::default() },
                Plugin { url: "owner/repo2".to_string(), lazy: false, merge: true, rev: None, ..Default::default() },
            ],
        };
        let output = format_plugin_list(&config);
        assert!(output.contains("owner/repo1"));
        assert!(output.contains("v1.0"));
        assert!(output.contains("Lazy"));
        assert!(output.contains("Eager"));
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
