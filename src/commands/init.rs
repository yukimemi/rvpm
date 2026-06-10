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

/// loader.lua を参照する `dofile(...)` 行を config から生成する。
/// 優先順位: `options.cache_root`/plugins/loader.lua > `~/.cache/rvpm/<appname>/plugins/loader.lua`
/// tilde 形式を保持することで dotfiles のマシン間共有を妨げない。
pub(crate) fn loader_init_snippet(config: &crate::config::Config) -> String {
    let raw_path = if let Some(base) = &config.options.cache_root {
        format!("{}/plugins/loader.lua", base.trim_end_matches(['/', '\\']))
    } else {
        format!("~/.cache/rvpm/{}/plugins/loader.lua", appname())
    };
    // Windows のバックスラッシュを Lua 文字列リテラルで安全な '/' に正規化。
    let raw_path = raw_path.replace('\\', "/");
    format!("dofile(vim.fn.expand(\"{}\"))", raw_path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_loader_init_snippet_uses_default_when_no_options() {
        let cfg = crate::config::Config {
            vars: None,
            options: crate::config::Options::default(),
            plugins: vec![],
        };
        let snippet = loader_init_snippet(&cfg);
        // appname は env 依存なので partial match
        assert!(snippet.starts_with("dofile(vim.fn.expand(\"~/.cache/rvpm/"));
        assert!(snippet.ends_with("/plugins/loader.lua\"))"));
    }

    #[test]
    fn test_loader_init_snippet_uses_cache_root_when_set() {
        let cfg = crate::config::Config {
            vars: None,
            options: crate::config::Options {
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
        let cfg = crate::config::Config {
            vars: None,
            options: crate::config::Options {
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
        let cfg = crate::config::Config {
            vars: None,
            options: crate::config::Options {
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
}
