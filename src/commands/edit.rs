use super::*;

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

/// global hooks 用のサークルアイコン。`init.lua` だけ Neovim 本体の場所
/// (`nvim_init_lua_path()`) を見て、`before.lua` / `after.lua` は `<config_root>`
/// 配下を見る — `rvpm edit --global` の対応と同じ。
fn global_hook_indicators(config_root: &Path, init_lua_path: &Path) -> String {
    let i = if init_lua_path.exists() {
        "\u{25cf}"
    } else {
        "\u{25cb}"
    };
    let b = if config_root.join("before.lua").exists() {
        "\u{25cf}"
    } else {
        "\u{25cb}"
    };
    let a = if config_root.join("after.lua").exists() {
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
