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

    for s in scripts {
        if let Some(init) = &s.init {
            lua.push_str(&format!("dofile(\"{}\")\n", init.replace("\\", "/")));
        }
    }

    let merged_path = merged_dir.to_string_lossy().replace("\\", "/");
    lua.push_str(&format!("\nvim.opt.rtp:append(\"{}\")\n\n", merged_path));

    for s in scripts {
        if !s.lazy {
            if let Some(before) = &s.before {
                lua.push_str(&format!("dofile(\"{}\")\n", before.replace("\\", "/")));
            }
            if let Some(after) = &s.after {
                lua.push_str(&format!("dofile(\"{}\")\n", after.replace("\\", "/")));
            }
        }
    }

    for s in scripts {
        if s.lazy {
            let path = s.path.replace("\\", "/");
            let before = s.before.as_ref().map(|p| format!("\"{}\"", p.replace("\\", "/"))).unwrap_or("nil".to_string());
            let after = s.after.as_ref().map(|p| format!("\"{}\"", p.replace("\\", "/"))).unwrap_or("nil".to_string());

            if let Some(cmds) = &s.on_cmd {
                for cmd in cmds {
                    lua.push_str(&format!(
                        "vim.api.nvim_create_user_command(\"{}\", function(opts)\n  vim.api.nvim_del_user_command(\"{}\")\n  load_lazy(\"{}\", \"{}\", {}, {})\n  vim.cmd(\"{} \" .. opts.args)\nend, {{ nargs = \"*\" }})\n",
                        cmd, cmd, s.name, path, before, after, cmd
                    ));
                }
            }

            if let Some(fts) = &s.on_ft {
                lua.push_str(&format!(
                    "vim.api.nvim_create_autocmd(\"FileType\", {{ pattern = {{ \"{}\" }}, once = true, callback = function()\n  load_lazy(\"{}\", \"{}\", {}, {})\nend }})\n",
                    fts.join("\", \""), s.name, path, before, after
                ));
            }

            if let Some(maps) = &s.on_map {
                for m in maps {
                    lua.push_str(&format!(
                        "vim.keymap.set(\"n\", \"{}\", function()\n  vim.keymap.del(\"n\", \"{}\")\n  load_lazy(\"{}\", \"{}\", {}, {})\n  vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes(\"{}\", true, true, true), \"m\", true)\nend)\n",
                        m, m, s.name, path, before, after, m
                    ));
                }
            }

            if let Some(events) = &s.on_event {
                lua.push_str(&format!(
                    "vim.api.nvim_create_autocmd({{ \"{}\" }}, {{ once = true, callback = function()\n  load_lazy(\"{}\", \"{}\", {}, {})\nend }})\n",
                    events.join("\", \""), s.name, path, before, after
                ));
            }
        }
    }

    lua
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_loader_complete() {
        let merged_dir = Path::new("/path/to/merged");
        let scripts = vec![
            PluginScripts {
                name: "full_lazy".to_string(),
                path: "/path/to/plugin".to_string(),
                init: None,
                before: None,
                after: None,
                lazy: true,
                on_cmd: Some(vec!["Cmd".to_string()]),
                on_ft: Some(vec!["rust".to_string()]),
                on_map: Some(vec!["<leader>f".to_string()]),
                on_event: Some(vec!["BufRead".to_string()]),
                on_path: None,
                on_source: None,
                cond: None,
            }
        ];
        let lua = generate_loader(merged_dir, &scripts);
        
        assert!(lua.contains("nvim_create_user_command(\"Cmd\""));
        assert!(lua.contains("pattern = { \"rust\" }"));
        assert!(lua.contains("vim.keymap.set(\"n\", \"<leader>f\""));
        assert!(lua.contains("nvim_create_autocmd({ \"BufRead\" }"));
    }
}
