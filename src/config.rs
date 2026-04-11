use serde::Deserialize;
use tera::{Tera, Context};
use anyhow::Result;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub vars: Option<serde_json::Value>,
    pub options: Options,
    pub plugins: Vec<Plugin>,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Default, Clone)]
pub struct Options {
    pub config_root: Option<String>,
    pub concurrency: Option<usize>,
    pub loader_path: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Default, Clone)]
pub struct Plugin {
    pub name: Option<String>,
    pub url: String,
    pub dst: Option<String>,
    #[serde(default)]
    pub lazy: bool,
    #[serde(default = "default_merge")]
    pub merge: bool,
    pub on_cmd: Option<Vec<String>>,
    pub on_ft: Option<Vec<String>>,
    pub on_map: Option<Vec<String>>,
    pub on_event: Option<Vec<String>>,
    pub depends: Option<Vec<String>>,
    pub build: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub rev: Option<String>,
}

fn default_merge() -> bool {
    true
}

impl Plugin {
    pub fn canonical_path(&self) -> String {
        let url = self.url.trim_end_matches(".git");
        if url.contains("://") {
            let parts: Vec<&str> = url.split("://").collect();
            let path = parts[1];
            path.to_string()
        } else if url.contains("@") {
            let parts: Vec<&str> = url.split("@").collect();
            let path = parts[1].replace(":", "/");
            path.to_string()
        } else {
            // owner/repo 形式とみなす
            if url.contains("/") {
                format!("github.com/{}", url)
            } else {
                url.to_string()
            }
        }
    }
}

pub fn parse_config(toml_str: &str) -> Result<Config> {
    // 1. Raw Parse
    #[derive(Deserialize)]
    struct Raw {
        vars: Option<serde_json::Value>,
    }
    let raw: Raw = toml::from_str(toml_str)?;

    // 2. Tera Context の構築
    let mut context = Context::new();
    if let Some(v) = raw.vars.as_ref() {
        context.insert("vars", v);
    }
    context.insert("is_windows", &cfg!(windows));

    // 環境変数の追加
    let mut env_map = std::collections::HashMap::new();
    for (key, value) in std::env::vars() {
        env_map.insert(key, value);
    }
    context.insert("env", &env_map);

    // 3. Tera でレンダリング
    let rendered = Tera::one_off(toml_str, &context, false)?;

    // 4. Final Parse
    let config: Config = toml::from_str(&rendered)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config_with_tera() {
        let toml_content = r#"
[vars]
base = "/tmp/rvpm"

[options]

[[plugins]]
name = "plenary"
url = "nvim-lua/plenary.nvim"
dst = "{{ vars.base }}/plenary"
"#;

        let config = parse_config(toml_content).unwrap();
        assert_eq!(config.plugins[0].dst, Some("/tmp/rvpm/plenary".to_string()));
    }

    #[test]
    fn test_parse_config_with_env_and_os() {
        unsafe { std::env::set_var("RVPM_TEST_ENV", "hello"); }
        let toml_content = r#"
[options]

[[plugins]]
name = "test"
url = "repo"
dst = "{{ env.RVPM_TEST_ENV }}_{{ is_windows }}"
"#;

        let config = parse_config(toml_content).unwrap();
        let expected_dst = format!("hello_{}", cfg!(windows));
        assert_eq!(config.plugins[0].dst, Some(expected_dst));
    }

    #[test]
    fn test_parse_complex_config() {
        let toml_content = r#"
[options]

[[plugins]]
url = "nvim-telescope/telescope.nvim"
lazy = true
on_cmd = ["Telescope"]
depends = ["plenary"]
merge = false
"#;
        let config = parse_config(toml_content).unwrap();
        let p = &config.plugins[0];
        assert_eq!(p.url, "nvim-telescope/telescope.nvim");
        assert!(p.lazy);
        assert_eq!(p.on_cmd, Some(vec!["Telescope".to_string()]));
        assert_eq!(p.depends, Some(vec!["plenary".to_string()]));
        assert!(!p.merge);
    }

    #[test]
    fn test_plugin_canonical_path() {
        let p1 = Plugin { url: "https://github.com/owner/repo".to_string(), ..Default::default() };
        assert_eq!(p1.canonical_path(), "github.com/owner/repo");

        let p2 = Plugin { url: "owner/repo".to_string(), ..Default::default() };
        assert_eq!(p2.canonical_path(), "github.com/owner/repo");

        let p3 = Plugin { url: "git@github.com:owner/repo.git".to_string(), ..Default::default() };
        assert_eq!(p3.canonical_path(), "github.com/owner/repo");
    }
}
