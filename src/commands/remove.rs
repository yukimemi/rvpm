use super::*;

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
