use anyhow::Result;
use serde::Deserialize;
use tera::{Context, Tera};

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
    /// rvpm のデータ置き場 root の上書き。
    /// 未指定なら `~/.cache/rvpm`。ここで指定すると repos / merged / loader.lua
    /// 全てがこの配下にまとまる。`loader_path` が別途指定されていればそちらが優先。
    /// `~/...` 形式を受け付ける。
    pub base_dir: Option<String>,
}

/// Keymap 仕様. TOML では文字列 (`"<leader>f"`) またはテーブル
/// (`{ lhs = "<leader>f", mode = ["n", "x"], desc = "..." }`) で記述可能。
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct MapSpec {
    pub lhs: String,
    /// 空の場合は normal mode (`"n"`) として扱う。
    pub mode: Vec<String>,
    pub desc: Option<String>,
}

impl MapSpec {
    /// 空 mode を `["n"]` に正規化した Vec を返す。
    pub fn modes_or_default(&self) -> Vec<String> {
        if self.mode.is_empty() {
            vec!["n".to_string()]
        } else {
            self.mode.clone()
        }
    }
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
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub on_cmd: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub on_ft: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_map_specs")]
    pub on_map: Option<Vec<MapSpec>>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub on_event: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub on_path: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub on_source: Option<Vec<String>>,
    pub depends: Option<Vec<String>>,
    pub build: Option<String>,
    pub rev: Option<String>,
    pub cond: Option<String>,
}

/// TOML 上で `on_cmd = "Foo"` (単一文字列) または `on_cmd = ["Foo", "Bar"]` (配列)
/// のどちらの形式でも受け付け、内部的には `Vec<String>` に正規化する。
fn deserialize_string_or_vec<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrVec {
        String(String),
        Vec(Vec<String>),
    }

    let opt = Option::<StringOrVec>::deserialize(deserializer)?;
    Ok(opt.map(|v| match v {
        StringOrVec::String(s) => vec![s],
        StringOrVec::Vec(v) => v,
    }))
}

/// Vec<String> を文字列形式 ("n") または配列形式 (["n", "x"]) で受ける。
fn deserialize_string_or_vec_vec<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SV {
        S(String),
        V(Vec<String>),
    }
    let sv: SV = Deserialize::deserialize(deserializer)?;
    Ok(match sv {
        SV::S(s) => vec![s],
        SV::V(v) => v,
    })
}

/// on_map の各要素を文字列 (`"<leader>f"`) またはテーブル (`{ lhs = "...", mode = ..., desc = "..." }`) で受け付ける。
fn deserialize_map_specs<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<MapSpec>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct MapSpecFull {
        lhs: String,
        #[serde(default, deserialize_with = "deserialize_string_or_vec_vec")]
        mode: Vec<String>,
        desc: Option<String>,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum MapSpecRaw {
        Simple(String),
        Full(MapSpecFull),
    }

    let opt = Option::<Vec<MapSpecRaw>>::deserialize(deserializer)?;
    Ok(opt.map(|v| {
        v.into_iter()
            .map(|raw| match raw {
                MapSpecRaw::Simple(lhs) => MapSpec {
                    lhs,
                    mode: Vec::new(),
                    desc: None,
                },
                MapSpecRaw::Full(full) => MapSpec {
                    lhs: full.lhs,
                    mode: full.mode,
                    desc: full.desc,
                },
            })
            .collect()
    }))
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

pub fn sort_plugins(plugins: &mut Vec<Plugin>) -> Result<()> {
    let mut sorted = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut visiting = std::collections::HashSet::new();

    let plugin_map: std::collections::HashMap<String, &Plugin> =
        plugins.iter().map(|p| (p.url.clone(), p)).collect();

    fn visit(
        url: &str,
        plugin_map: &std::collections::HashMap<String, &Plugin>,
        visited: &mut std::collections::HashSet<String>,
        visiting: &mut std::collections::HashSet<String>,
        sorted: &mut Vec<Plugin>,
    ) -> Result<()> {
        if visited.contains(url) {
            return Ok(());
        }
        if visiting.contains(url) {
            eprintln!("Warning: Cyclic dependency detected: {}", url);
            return Ok(());
        }

        visiting.insert(url.to_string());

        if let Some(plugin) = plugin_map.get(url) {
            if let Some(deps) = &plugin.depends {
                for dep in deps {
                    visit(dep, plugin_map, visited, visiting, sorted)?;
                }
            }
            visited.insert(url.to_string());
            visiting.remove(url);
            sorted.push((*plugin).clone());
        } else {
            eprintln!("Warning: Dependency not found in config: {}", url);
            visited.insert(url.to_string());
            visiting.remove(url);
        }
        Ok(())
    }

    for plugin in plugins.iter() {
        visit(
            &plugin.url,
            &plugin_map,
            &mut visited,
            &mut visiting,
            &mut sorted,
        )?;
    }

    *plugins = sorted;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config_accepts_on_cmd_as_string() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_cmd = "Telescope"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(
            config.plugins[0].on_cmd,
            Some(vec!["Telescope".to_string()])
        );
    }

    #[test]
    fn test_parse_config_accepts_on_cmd_as_array() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_cmd = ["Telescope", "Grep"]
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(
            config.plugins[0].on_cmd,
            Some(vec!["Telescope".to_string(), "Grep".to_string()])
        );
    }

    #[test]
    fn test_parse_config_accepts_base_dir_option() {
        let toml = r#"
[options]
base_dir = "~/dotfiles/nvim/rvpm"

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(
            config.options.base_dir.as_deref(),
            Some("~/dotfiles/nvim/rvpm")
        );
    }

    #[test]
    fn test_parse_config_base_dir_defaults_to_none() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.options.base_dir, None);
    }

    #[test]
    fn test_parse_config_accepts_on_map_simple_string() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_map = ["<leader>f"]
"#;
        let config = parse_config(toml).unwrap();
        let maps = config.plugins[0].on_map.as_ref().unwrap();
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].lhs, "<leader>f");
        assert!(
            maps[0].mode.is_empty(),
            "simple form leaves mode empty (defaults to 'n' at generate)"
        );
        assert_eq!(maps[0].desc, None);
    }

    #[test]
    fn test_parse_config_accepts_on_map_table_form() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_map = [
  { lhs = "<leader>v", mode = ["n", "x"], desc = "visual thing" },
]
"#;
        let config = parse_config(toml).unwrap();
        let maps = config.plugins[0].on_map.as_ref().unwrap();
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].lhs, "<leader>v");
        assert_eq!(maps[0].mode, vec!["n".to_string(), "x".to_string()]);
        assert_eq!(maps[0].desc.as_deref(), Some("visual thing"));
    }

    #[test]
    fn test_parse_config_accepts_on_map_mixed_forms() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_map = [
  "<leader>a",
  { lhs = "<leader>b", mode = "x" },
  { lhs = "<leader>c", mode = ["n", "v"], desc = "C" },
]
"#;
        let config = parse_config(toml).unwrap();
        let maps = config.plugins[0].on_map.as_ref().unwrap();
        assert_eq!(maps.len(), 3);
        assert_eq!(maps[0].lhs, "<leader>a");
        assert!(maps[0].mode.is_empty());
        assert_eq!(maps[1].lhs, "<leader>b");
        assert_eq!(maps[1].mode, vec!["x".to_string()]);
        assert_eq!(maps[2].lhs, "<leader>c");
        assert_eq!(maps[2].mode, vec!["n".to_string(), "v".to_string()]);
        assert_eq!(maps[2].desc.as_deref(), Some("C"));
    }

    #[test]
    fn test_parse_config_accepts_on_event_as_string() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_event = "BufReadPre"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(
            config.plugins[0].on_event,
            Some(vec!["BufReadPre".to_string()])
        );
    }

    #[test]
    fn test_sort_plugins_dependencies() {
        let mut plugins = vec![
            Plugin {
                url: "A".to_string(),
                depends: Some(vec!["B".to_string()]),
                ..Default::default()
            },
            Plugin {
                url: "B".to_string(),
                ..Default::default()
            },
        ];

        sort_plugins(&mut plugins).unwrap();

        assert_eq!(plugins[0].url, "B");
        assert_eq!(plugins[1].url, "A");
    }

    #[test]
    fn test_sort_plugins_cycle_resilience() {
        let mut plugins = vec![
            Plugin {
                url: "A".to_string(),
                depends: Some(vec!["B".to_string()]),
                ..Default::default()
            },
            Plugin {
                url: "B".to_string(),
                depends: Some(vec!["A".to_string()]),
                ..Default::default()
            },
            Plugin {
                url: "C".to_string(),
                ..Default::default()
            },
        ];

        let result = sort_plugins(&mut plugins);
        assert!(result.is_ok());
        assert!(plugins.iter().any(|p| p.url == "C"));
    }

    #[test]
    fn test_sort_plugins_missing_dependency_resilience() {
        let mut plugins = vec![Plugin {
            url: "A".to_string(),
            depends: Some(vec!["NOT_FOUND".to_string()]),
            ..Default::default()
        }];

        let result = sort_plugins(&mut plugins);
        // エラーにならずに成功すべき
        assert!(result.is_ok());
        // A はリストに残るべき
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].url, "A");
    }

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
        unsafe {
            std::env::set_var("RVPM_TEST_ENV", "hello");
        }
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
on_path = ["*.rs"]
on_source = ["plenary.nvim"]
depends = ["plenary"]
merge = false
"#;
        let config = parse_config(toml_content).unwrap();
        let p = &config.plugins[0];
        assert_eq!(p.url, "nvim-telescope/telescope.nvim");
        assert!(p.lazy);
        assert_eq!(p.on_cmd, Some(vec!["Telescope".to_string()]));
        assert_eq!(p.on_path, Some(vec!["*.rs".to_string()]));
        assert_eq!(p.on_source, Some(vec!["plenary.nvim".to_string()]));
        assert_eq!(p.depends, Some(vec!["plenary".to_string()]));
        assert!(!p.merge);
    }

    #[test]
    fn test_plugin_canonical_path() {
        let p1 = Plugin {
            url: "https://github.com/owner/repo".to_string(),
            ..Default::default()
        };
        assert_eq!(p1.canonical_path(), "github.com/owner/repo");

        let p2 = Plugin {
            url: "owner/repo".to_string(),
            ..Default::default()
        };
        assert_eq!(p2.canonical_path(), "github.com/owner/repo");

        let p3 = Plugin {
            url: "git@github.com:owner/repo.git".to_string(),
            ..Default::default()
        };
        assert_eq!(p3.canonical_path(), "github.com/owner/repo");
    }
}
