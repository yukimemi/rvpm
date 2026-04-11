use std::path::Path;
use anyhow::Result;
use crate::config::Plugin;

#[derive(Clone)]
pub struct PluginScripts {
    pub name: String,
    pub path: String,
    pub init: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub lazy: bool,
    pub on_cmd: Option<Vec<String>>,
    pub on_ft: Option<Vec<String>>,
    pub on_map: Option<Vec<String>>,
    pub on_event: Option<Vec<String>>,
    pub on_path: Option<Vec<String>>,
    pub on_source: Option<Vec<String>>,
    pub cond: Option<String>,
}

pub fn generate_loader(merged_dir: &Path, scripts: &[PluginScripts]) -> String {
    let mut lua = String::new();
    lua.push_str("-- rvpm generated loader.lua\n\n");

    lua.push_str(r#"
local function load_lazy(name, path, before, after)
  if _G["rvpm_loaded_" .. name] then return end
  _G["rvpm_loaded_" .. name] = true
  vim.opt.rtp:append(path)
  if before then dofile(before) end
  local plugin_files = vim.fn.glob(path .. "/plugin/**/*.{vim,lua}", false, true)
  for _, file in ipairs(plugin_files) do
    vim.cmd("source " .. file)
  end
  if after then dofile(after) end
end
"#);
    lua.push_str("\n");

    // 1. All init.lua (Always run)
    for s in scripts {
        if let Some(init) = &s.init {
            lua.push_str(&format!("dofile(\"{}\")\n", init.replace("\\", "/")));
        }
    }

    // 2. RTP append (Merged)
    let merged_path = merged_dir.to_string_lossy().replace("\\", "/");
    lua.push_str(&format!("\nvim.opt.rtp:append(\"{}\")\n\n", merged_path));

    // 3. Eager plugins
    for s in scripts {
        if !s.lazy {
            let mut setup_lua = String::new();
            if let Some(before) = &s.before {
                setup_lua.push_str(&format!("dofile(\"{}\")\n", before.replace("\\", "/")));
            }
            if let Some(after) = &s.after {
                setup_lua.push_str(&format!("dofile(\"{}\")\n", after.replace("\\", "/")));
            }

            if let Some(c) = &s.cond {
                lua.push_str(&format!("if {} then\n", c));
                lua.push_str(&setup_lua);
                lua.push_str("end\n");
            } else {
                lua.push_str(&setup_lua);
            }
        }
    }

    // 4. Lazy plugins
    for s in scripts {
        if s.lazy {
            let path = s.path.replace("\\", "/");
            let before = s.before.as_ref().map(|p| format!("\"{}\"", p.replace("\\", "/"))).unwrap_or("nil".to_string());
            let after = s.after.as_ref().map(|p| format!("\"{}\"", p.replace("\\", "/"))).unwrap_or("nil".to_string());

            let mut trigger_lua = String::new();

            if let Some(cmds) = &s.on_cmd {
                for cmd in cmds {
                    trigger_lua.push_str(&format!(
                        "vim.api.nvim_create_user_command(\"{}\", function(opts)\n  vim.api.nvim_del_user_command(\"{}\")\n  load_lazy(\"{}\", \"{}\", {}, {})\n  vim.cmd(\"{} \" .. opts.args)\nend, {{ nargs = \"*\" }})\n",
                        cmd, cmd, s.name, path, before, after, cmd
                    ));
                }
            }

            if let Some(fts) = &s.on_ft {
                trigger_lua.push_str(&format!(
                    "vim.api.nvim_create_autocmd(\"FileType\", {{ pattern = {{ \"{}\" }}, once = true, callback = function()\n  load_lazy(\"{}\", \"{}\", {}, {})\nend }})\n",
                    fts.join("\", \""), s.name, path, before, after
                ));
            }

            if let Some(maps) = &s.on_map {
                for m in maps {
                    trigger_lua.push_str(&format!(
                        "vim.keymap.set(\"n\", \"{}\", function()\n  vim.keymap.del(\"n\", \"{}\")\n  load_lazy(\"{}\", \"{}\", {}, {})\n  vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes(\"{}\", true, true, true), \"m\", true)\nend)\n",
                        m, m, s.name, path, before, after, m
                    ));
                }
            }

            if let Some(events) = &s.on_event {
                trigger_lua.push_str(&format!(
                    "vim.api.nvim_create_autocmd({{ \"{}\" }}, {{ once = true, callback = function()\n  load_lazy(\"{}\", \"{}\", {}, {})\nend }})\n",
                    events.join("\", \""), s.name, path, before, after
                ));
            }

            if let Some(paths) = &s.on_path {
                trigger_lua.push_str(&format!(
                    "vim.api.nvim_create_autocmd({{ \"BufRead\", \"BufNewFile\" }}, {{ pattern = {{ \"{}\" }}, once = true, callback = function()\n  load_lazy(\"{}\", \"{}\", {}, {})\nend }})\n",
                    paths.join("\", \""), s.name, path, before, after
                ));
            }

            if let Some(sources) = &s.on_source {
                let patterns: Vec<String> = sources.iter().map(|src| format!("rvpm_loaded_{}", src)).collect();
                trigger_lua.push_str(&format!(
                    "vim.api.nvim_create_autocmd(\"User\", {{ pattern = {{ \"{}\" }}, once = true, callback = function()\n  load_lazy(\"{}\", \"{}\", {}, {})\nend }})\n",
                    patterns.join("\", \""), s.name, path, before, after
                ));
            }

            if let Some(c) = &s.cond {
                lua.push_str(&format!("if {} then\n", c));
                lua.push_str(&trigger_lua);
                lua.push_str("end\n");
            } else {
                lua.push_str(&trigger_lua);
            }
        }
    }

    lua
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_loader_with_cond() {
        let merged_dir = Path::new("/path/to/merged");
        let scripts = vec![
            PluginScripts {
                name: "cond_lazy".to_string(),
                path: "/path/to/plugin".to_string(),
                init: None,
                before: None,
                after: None,
                lazy: true,
                on_cmd: Some(vec!["Cmd".to_string()]),
                on_ft: Some(vec!["rust".to_string()]),
                on_map: Some(vec!["<leader>f".to_string()]),
                on_event: Some(vec!["BufRead".to_string()]),
                on_path: Some(vec!["*.rs".to_string(), "Cargo.toml".to_string()]),
                on_source: Some(vec!["plenary.nvim".to_string()]),
                cond: Some("vim.fn.has('win32') == 1".to_string()),
                }
                ];
                let lua = generate_loader(merged_dir, &scripts);

                assert!(lua.contains("if vim.fn.has('win32') == 1 then"));
                assert!(lua.contains("nvim_create_user_command(\"Cmd\""));
                assert!(lua.contains("pattern = { \"rust\" }"));
                assert!(lua.contains("vim.keymap.set(\"n\", \"<leader>f\""));
                assert!(lua.contains("nvim_create_autocmd({ \"BufRead\" }"));
                // on_path の確認 (BufRead, BufNewFile で pattern 指定)
                assert!(lua.contains("pattern = { \"*.rs\", \"Cargo.toml\" }"));
                // on_source の確認 (User イベントで pattern が rvpm_loaded_... となる想定)
                assert!(lua.contains("pattern = { \"rvpm_loaded_plenary.nvim\" }"));
                }
                }

