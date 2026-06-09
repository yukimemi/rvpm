use super::*;

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
