mod config;
mod git;
mod link;
mod loader;

use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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
    /// Sync plugins based on config.toml (Clone/Pull/Merge/Generate)
    Sync,
    /// Add a new plugin to config.toml
    Add {
        /// Repository URL (e.g., owner/repo or full URL)
        repo: String,
        /// Optional name for the plugin
        #[arg(long)]
        name: Option<String>,
    },
    /// Edit plugin configuration files (init/before/after.lua)
    Edit {
        /// Query to filter plugins
        query: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Sync => run_sync().await?,
        Commands::Add { repo, name } => run_add(repo, name).await?,
        Commands::Edit { query } => run_edit(query).await?,
    }

    Ok(())
}

async fn run_sync() -> Result<()> {
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

    let config = parse_config(&toml_content)?;
    
    let base_dir = home.join(".cache/rvpm");
    let merged_dir = base_dir.join("merged");
    
    if merged_dir.exists() {
        let _ = std::fs::remove_dir_all(&merged_dir);
    }
    std::fs::create_dir_all(&merged_dir)?;

    println!("Using config: {}", config_path.display());
    println!("Syncing plugins...");

    let mut plugin_scripts = Vec::new();

    for plugin in &config.plugins {
        let dst_path = if let Some(d) = &plugin.dst {
            PathBuf::from(d)
        } else {
            base_dir.join("repos").join(plugin.canonical_path())
        };

        let repo = Repo::new(&plugin.url, &dst_path);
        println!("  Updating {}...", plugin.url);
        repo.sync().await?;

        if plugin.merge {
            println!("  Merging {}...", plugin.url);
            merge_plugin(&dst_path, &merged_dir)?;
        }
// 規約ファイルの探索
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
    };

    plugin_scripts.push(scripts);
}

    }

    println!("Generating loader.lua...");
    let lua = generate_loader(&merged_dir, &plugin_scripts);
    let loader_path = base_dir.join("loader.lua");
    std::fs::write(loader_path, lua)?;

    println!("Done!");
    Ok(())
}

fn find_lua(dir: &Path, name: &str) -> Option<String> {
    let path = dir.join(name);
    if path.exists() {
        Some(path.to_string_lossy().to_string())
    } else {
        None
    }
}

use toml_edit::{DocumentMut, table, value, Item};

async fn run_add(repo: String, name: Option<String>) -> Result<()> {
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)?;
    
    let mut doc = toml_content.parse::<DocumentMut>()?;
    
    if doc.get("plugins").is_none() {
        doc["plugins"] = toml_edit::ArrayOfTables::new().into();
    }
    
    let plugins = doc["plugins"].as_array_of_tables_mut().context("plugins is not an array of tables")?;
    
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
    
    // table() は Item を返すので、Table に変換してから push
    if let Item::Table(t) = new_plugin {
        plugins.push(t);
    }
    
    std::fs::write(&config_path, doc.to_string())?;
    println!("Added plugin to config: {}", repo);
    
    run_sync().await?;
    Ok(())
}

use dialoguer::{FuzzySelect, Select};

async fn run_edit(query: Option<String>) -> Result<()> {
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    // 1. プラグインの選択
    let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
    
    let selection = FuzzySelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Select plugin to edit")
        .default(0)
        .items(&urls)
        .interact_opt()?;

    let selected_url = match selection {
        Some(index) => &urls[index],
        None => return Ok(()),
    };

    let plugin = config.plugins.iter().find(|p| &p.url == selected_url).unwrap();

    // 2. 編集するファイルの選択
    let files = vec!["init.lua", "before.lua", "after.lua"];
    let file_selection = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Select file to edit")
        .default(0)
        .items(&files)
        .interact_opt()?;

    let file_name = match file_selection {
        Some(index) => files[index],
        None => return Ok(()),
    };
    
    // 3. ファイルを開く
    if let Some(config_root) = &config.options.config_root {
        let plugin_config_dir = Path::new(config_root).join(plugin.canonical_path());
        std::fs::create_dir_all(&plugin_config_dir)?;
        let target_file = plugin_config_dir.join(file_name);

        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
        std::process::Command::new(editor)
            .arg(target_file)
            .status()?;
    }

    run_sync().await?;
    Ok(())
}
