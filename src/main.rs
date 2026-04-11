mod config;
mod git;
mod link;
mod loader;

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
    /// Set plugin options (lazy, merge, on_cmd, etc.)
    Set {
        /// Query to filter plugins
        query: Option<String>,
        /// Enable or disable lazy loading
        #[arg(long)]
        lazy: Option<bool>,
        /// Enable or disable merging
        #[arg(long)]
        merge: Option<bool>,
        /// Commands for lazy loading (as JSON array string)
        #[arg(long)]
        on_cmd: Option<String>,
        /// FileTypes for lazy loading (as JSON array string)
        #[arg(long)]
        on_ft: Option<String>,
        /// Branch to checkout
        #[arg(long)]
        branch: Option<String>,
        /// Tag to checkout
        #[arg(long)]
        tag: Option<String>,
        /// Revision to checkout
        #[arg(long)]
        rev: Option<String>,
    },
    /// Remove unused plugin directories from repos/
    Clean {
        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Sync => run_sync().await?,
        Commands::Add { repo, name } => run_add(repo, name).await?,
        Commands::Edit { query } => run_edit(query).await?,
        Commands::Set { query, lazy, merge, on_cmd, on_ft, branch, tag, rev } => {
            run_set(query, lazy, merge, on_cmd, on_ft, branch, tag, rev).await?
        },
        Commands::Clean { force } => run_clean(force).await?,
    }

    Ok(())
}

async fn run_clean(force: bool) -> Result<()> {
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;
    
    let base_dir = home.join(".cache/rvpm");
    let repos_dir = base_dir.join("repos");

    let unused = find_unused_repos(&config, &repos_dir)?;

    if unused.is_empty() {
        println!("No unused plugins found.");
        return Ok(());
    }

    println!("Found unused plugin directories:");
    for path in &unused {
        println!("  {}", path.display());
    }

    let confirm = if force {
        true
    } else {
        dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Do you want to delete these directories?")
            .default(false)
            .interact()?
    };

    if confirm {
        for path in unused {
            println!("Deleting {}...", path.display());
            let _ = std::fs::remove_dir_all(path);
        }
        println!("Cleanup complete.");
    }

    Ok(())
}

async fn run_sync() -> Result<()> {
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

    let config = Arc::new(parse_config(&toml_content)?);
    
    let base_dir = home.join(".cache/rvpm");
    let merged_dir = base_dir.join("merged");
    
    if merged_dir.exists() {
        let _ = std::fs::remove_dir_all(&merged_dir);
    }
    std::fs::create_dir_all(&merged_dir)?;

    println!("Using config: {}", config_path.display());
    println!("Syncing plugins...");

    let mut set = JoinSet::new();

    for plugin in config.plugins.iter() {
        let plugin = plugin.clone();
        let base_dir = base_dir.clone();
        
        set.spawn(async move {
            let dst_path = if let Some(d) = &plugin.dst {
                PathBuf::from(d)
            } else {
                base_dir.join("repos").join(plugin.canonical_path())
            };

            let repo = Repo::new(&plugin.url, &dst_path);
            repo.sync().await.map(|_| (plugin, dst_path))
        });
    }

    let mut plugin_scripts = Vec::new();
    while let Some(res) = set.join_next().await {
        let (plugin, dst_path) = res??;
        println!("  Finished syncing {}...", plugin.url);

        if plugin.merge {
            println!("  Merging {}...", plugin.url);
            merge_plugin(&dst_path, &merged_dir)?;
        }

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
                on_path: None, // 未対応
                on_source: None, // 未対応
                cond: None, // 未対応
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

    let urls: Vec<String> = config.plugins.iter().map(|p| p.url.clone()).collect();
    
    let selection = FuzzySelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Select plugin to edit")
        .with_initial_text(query.unwrap_or_default())
        .items(&urls)
        .interact_opt()?;

    let selected_url = match selection {
        Some(index) => &urls[index],
        None => return Ok(()),
    };

    let plugin = config.plugins.iter().find(|p| &p.url == selected_url).unwrap();

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

async fn run_set(
    query: Option<String>, 
    lazy: Option<bool>, 
    merge: Option<bool>, 
    on_cmd: Option<String>,
    on_ft: Option<String>,
    branch: Option<String>,
    tag: Option<String>,
    rev: Option<String>,
) -> Result<()> {
    let home = dirs::home_dir().expect("Could not find home directory");
    let config_path = home.join(".config/rvpm/config.toml");
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    // 1. プラグインの選択
    let selected_repo_url = if let Some(q) = query.as_ref() {
        let matches: Vec<_> = config.plugins.iter().filter(|p| p.url.contains(q)).collect();
        if matches.len() == 1 {
            matches[0].url.clone()
        } else {
            select_plugin_interactively(&config.plugins, Some(q))?
        }
    } else {
        select_plugin_interactively(&config.plugins, None)?
    };

    let mut doc = toml_content.parse::<DocumentMut>()?;
    let plugins = doc["plugins"].as_array_of_tables_mut().context("plugins is not an array of tables")?;

    let plugin_table = plugins.iter_mut().find(|p| p.get("url").and_then(|v| v.as_str()) == Some(&selected_repo_url))
        .context("Could not find plugin in toml_edit document")?;

    if lazy.is_some() || merge.is_some() || on_cmd.is_some() || on_ft.is_some() || branch.is_some() || tag.is_some() || rev.is_some() {
        let parse_list = |s: Option<String>| -> Option<Vec<String>> {
            s.map(|v| {
                if v.trim().starts_with('[') {
                    serde_json::from_str(&v).unwrap_or_else(|_| vec![v])
                } else {
                    v.split(',').map(|s| s.trim().to_string()).collect()
                }
            })
        };

        update_plugin_config(
            &mut doc,
            &selected_repo_url,
            lazy,
            merge,
            parse_list(on_cmd),
            parse_list(on_ft),
        )?;
    } else {
        // インタラクティブ設定モード
        let options = vec!["lazy", "merge", "on_cmd", "on_ft"];
        let selection = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Select option to set")
            .items(&options)
            .interact_opt()?;

        if let Some(index) = selection {
            match options[index] {
                "lazy" | "merge" => {
                    let val = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                        .with_prompt(format!("Set {} to", options[index]))
                        .items(&["true", "false"])
                        .interact()?;
                    update_plugin_config(
                        &mut doc,
                        &selected_repo_url,
                        if options[index] == "lazy" { Some(val == 0) } else { None },
                        if options[index] == "merge" { Some(val == 0) } else { None },
                        None,
                        None,
                    )?;
                }
                "on_cmd" | "on_ft" => {
                    let val: String = dialoguer::Input::new()
                        .with_prompt(format!("Enter {} (comma separated)", options[index]))
                        .interact_text()?;
                    let cmds: Vec<String> = val.split(',').map(|s| s.trim().to_string()).collect();
                    update_plugin_config(
                        &mut doc,
                        &selected_repo_url,
                        None,
                        None,
                        if options[index] == "on_cmd" { Some(cmds.clone()) } else { None },
                        if options[index] == "on_ft" { Some(cmds) } else { None },
                    )?;
                }
                _ => {}
            }
        }
    }

    std::fs::write(&config_path, doc.to_string())?;
    println!("Updated config for: {}", selected_repo_url);
    
    run_sync().await?;
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

    for plugin in &config.plugins {
        used_paths.insert(repos_dir.join(plugin.canonical_path()));
    }

    for entry in walkdir::WalkDir::new(repos_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() == ".git")
    {
        let git_dir = entry.path();
        if let Some(repo_root) = git_dir.parent() {
            if !used_paths.contains(repo_root) {
                unused.push(repo_root.to_path_buf());
            }
        }
    }

    Ok(unused)
}

fn update_plugin_config(
    doc: &mut DocumentMut,
    url: &str,
    lazy: Option<bool>,
    merge: Option<bool>,
    on_cmd: Option<Vec<String>>,
    on_ft: Option<Vec<String>>,
) -> Result<()> {
    let plugins = doc["plugins"].as_array_of_tables_mut().context("plugins is not an array of tables")?;

    let plugin_table = plugins.iter_mut().find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
        .context("Could not find plugin in toml_edit document")?;

    if let Some(l) = lazy { plugin_table["lazy"] = value(l); }
    if let Some(m) = merge { plugin_table["merge"] = value(m); }
    
    if let Some(cmds) = on_cmd {
        let mut array = toml_edit::Array::new();
        for cmd in cmds { array.push(cmd); }
        plugin_table["on_cmd"] = value(array);
    }
    
    if let Some(fts) = on_ft {
        let mut array = toml_edit::Array::new();
        for ft in fts { array.push(ft); }
        plugin_table["on_ft"] = value(array);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Plugin, Options};
    use tempfile::tempdir;
    use toml_edit::DocumentMut;

    #[test]
    fn test_update_plugin_config() {
        let toml = r#"
[[plugins]]
url = "test/plugin"
lazy = false
"#;
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        
        update_plugin_config(&mut doc, "test/plugin", Some(true), Some(true), None, None).unwrap();
        
        let result = doc.to_string();
        assert!(result.contains("lazy = true"));
        assert!(result.contains("merge = true"));
    }

    #[test]
    fn test_find_unused_repos() {
        let root = tempdir().unwrap();
        let repos_dir = root.path().join("repos");
        std::fs::create_dir_all(&repos_dir).unwrap();

        // 存在するディレクトリ
        let used_dir = repos_dir.join("github.com/used/plugin");
        let unused_dir = repos_dir.join("github.com/unused/plugin");
        std::fs::create_dir_all(used_dir.join(".git")).unwrap();
        std::fs::create_dir_all(unused_dir.join(".git")).unwrap();

        // Config には used しかない
        let config = Config {
            vars: None,
            options: Options::default(),
            plugins: vec![
                Plugin {
                    url: "used/plugin".to_string(),
                    ..Default::default()
                }
            ],
        };

        let unused = find_unused_repos(&config, &repos_dir).unwrap();
        
        assert_eq!(unused.len(), 1);
        assert!(unused[0].to_string_lossy().contains("unused"));
    }
}
