use std::path::Path;
use anyhow::Result;
use crate::config::Plugin;

pub struct PluginScripts {
    pub name: String,
    pub path: String, // プラグインの実体パス
    pub init: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub lazy: bool,
    pub on_cmd: Option<Vec<String>>,
    pub on_ft: Option<Vec<String>>,
}

pub fn generate_loader(merged_dir: &Path, scripts: &[PluginScripts]) -> String {
    let mut lua = String::new();
    lua.push_str("-- rvpm generated loader.lua\n\n");

    // 共通の遅延読み込み関数
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

    // 1. All init.lua (Before RTP append)
    for s in scripts {
        if let Some(init) = &s.init {
            lua.push_str(&format!("dofile(\"{}\")\n", init.replace("\\", "/")));
        }
    }

    // 2. RTP append (Eager only)
    let merged_path = merged_dir.to_string_lossy().replace("\\", "/");
    lua.push_str(&format!("\nvim.opt.rtp:append(\"{}\")\n\n", merged_path));

    // 3. Eager plugins: before & after
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

    // 4. Lazy plugins: Setup triggers
    for s in scripts {
        if s.lazy {
            let path = s.path.replace("\\", "/");
            let before = s.before.as_ref().map(|p| format!("\"{}\"", p.replace("\\", "/"))).unwrap_or("nil".to_string());
            let after = s.after.as_ref().map(|p| format!("\"{}\"", p.replace("\\", "/"))).unwrap_or("nil".to_string());

            // on_cmd
            if let Some(cmds) = &s.on_cmd {
                for cmd in cmds {
                    lua.push_str(&format!(
                        "vim.api.nvim_create_user_command(\"{}\", function(opts)\n  vim.api.nvim_del_user_command(\"{}\")\n  load_lazy(\"{}\", \"{}\", {}, {})\n  vim.cmd(\"{} \" .. opts.args)\nend, {{ nargs = \"*\" }})\n",
                        cmd, cmd, s.name, path, before, after, cmd
                    ));
                }
            }

            // on_ft
            if let Some(fts) = &s.on_ft {
                lua.push_str(&format!(
                    "vim.api.nvim_create_autocmd(\"FileType\", {{ pattern = {{ \"{}\" }}, once = true, callback = function()\n  load_lazy(\"{}\", \"{}\", {}, {})\nend }})\n",
                    fts.join("\", \""), s.name, path, before, after
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
    fn test_generate_loader_with_lazy() {
        let merged_dir = Path::new("/path/to/merged");
        let scripts = vec![
            PluginScripts {
                name: "telescope".to_string(),
                path: "/path/to/telescope".to_string(),
                init: None,
                before: None,
                after: Some("/path/to/after.lua".to_string()),
                lazy: true,
                on_cmd: Some(vec!["Telescope".to_string()]),
                on_ft: Some(vec!["rust".to_string()]),
            }
        ];
        let lua = generate_loader(merged_dir, &scripts);
        
        assert!(lua.contains("nvim_create_user_command(\"Telescope\""));
        assert!(lua.contains("nvim_create_autocmd(\"FileType\", { pattern = { \"rust\" }"));
        assert!(lua.contains("load_lazy(\"telescope\""));
    }
}
