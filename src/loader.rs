use crate::config::MapSpec;
use std::path::Path;

#[derive(Clone)]
pub struct PluginScripts {
    pub name: String,
    pub path: String,
    pub merge: bool,
    pub init: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
    /// 事前コンパイル: plugin/**/*.{vim,lua} のファイルパス (ソート済み)
    pub plugin_files: Vec<String>,
    /// 事前コンパイル: ftdetect/**/*.{vim,lua} のファイルパス (ソート済み)
    /// augroup filetypedetect 内で source する必要がある
    pub ftdetect_files: Vec<String>,
    /// 事前コンパイル: after/plugin/**/*.{vim,lua} のファイルパス (ソート済み)
    pub after_plugin_files: Vec<String>,
    pub lazy: bool,
    pub on_cmd: Option<Vec<String>>,
    pub on_ft: Option<Vec<String>>,
    pub on_map: Option<Vec<MapSpec>>,
    pub on_event: Option<Vec<String>>,
    pub on_path: Option<Vec<String>>,
    pub on_source: Option<Vec<String>>,
    pub depends: Option<Vec<String>>,
    /// 事前コンパイル: colors/*.{vim,lua} からファイル名 (拡張子なし) を抽出したカラースキーム名
    pub colorschemes: Vec<String>,
    pub cond: Option<String>,
}

impl PluginScripts {
    /// テスト用のデフォルト値コンストラクタ (本番コードでは使わない想定)
    #[cfg(test)]
    pub fn for_test(name: &str, path: &str) -> Self {
        Self {
            name: name.to_string(),
            path: path.to_string(),
            merge: true,
            init: None,
            before: None,
            after: None,
            plugin_files: Vec::new(),
            ftdetect_files: Vec::new(),
            after_plugin_files: Vec::new(),
            lazy: false,
            on_cmd: None,
            on_ft: None,
            on_map: None,
            on_event: None,
            on_path: None,
            on_source: None,
            depends: None,
            colorschemes: Vec::new(),
            cond: None,
        }
    }
}

/// Lua のリスト literal に変換 (`{ "a", "b" }` 形式)
fn lua_str_list(items: &[String]) -> String {
    if items.is_empty() {
        return "{}".to_string();
    }
    let quoted: Vec<String> = items
        .iter()
        .map(|s| format!("\"{}\"", s.replace('\\', "/")))
        .collect();
    format!("{{ {} }}", quoted.join(", "))
}

/// ローカル lua 変数名として安全な形に sanitize (英数字 + underscore のみ)
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn push_with_cond(lua: &mut String, cond: &Option<String>, body: &str) {
    if let Some(c) = cond {
        lua.push_str(&format!("if {} then\n", c));
        lua.push_str(body);
        lua.push_str("end\n");
    } else {
        lua.push_str(body);
    }
}

/// generate_loader に渡すグローバルオプション。
#[derive(Default)]
pub struct LoaderOptions {
    /// `~/.config/rvpm/before.lua` が存在すれば Some (グローバル before.lua)
    pub global_before: Option<String>,
    /// `~/.config/rvpm/after.lua` が存在すれば Some (グローバル after.lua)
    pub global_after: Option<String>,
}

/// lazy → eager 自動昇格を行い、昇格されたプラグイン名のリストを返す。
///
/// 以下のケースで lazy を eager に昇格する:
///   1. eager が lazy に depends → lazy を eager に
///   2. lazy が on_source で eager を参照 → その lazy は phase 6 後の
///      User autocmd を受けないと永遠にロードされないので eager に昇格
///
/// チェーン対応: A(eager) ← B(lazy, on_source=["A"]) ← C(lazy, on_source=["B"])
/// → B 昇格 → C も昇格。ループで収束するまで繰り返す。
pub fn promote_lazy_to_eager(scripts: &mut [PluginScripts]) -> Vec<String> {
    let mut promoted = Vec::new();
    let max_iterations = scripts.len() + 1;
    for _ in 0..max_iterations {
        let eager_names: std::collections::HashSet<String> = scripts
            .iter()
            .filter(|s| !s.lazy)
            .map(|s| s.name.clone())
            .collect();

        let depended_by_eager: std::collections::HashSet<String> = scripts
            .iter()
            .filter(|s| !s.lazy)
            .flat_map(|s| s.depends.iter().flatten().cloned())
            .collect();

        let to_promote: Vec<(String, &'static str)> = scripts
            .iter()
            .filter(|s| s.lazy)
            .filter_map(|s| {
                if depended_by_eager.contains(&s.name) {
                    Some((s.name.clone(), "depended on by an eager plugin"))
                } else if s
                    .on_source
                    .as_ref()
                    .map(|sources| sources.iter().any(|src| eager_names.contains(src)))
                    .unwrap_or(false)
                {
                    Some((
                        s.name.clone(),
                        "on_source references an eager plugin (event fires before listener is registered)",
                    ))
                } else {
                    None
                }
            })
            .collect();

        if to_promote.is_empty() {
            break;
        }

        for (name, reason) in &to_promote {
            eprintln!(
                "Note: '{}' is lazy but {} — promoting to eager.",
                name, reason
            );
            if let Some(s) = scripts.iter_mut().find(|s| s.name == *name) {
                s.lazy = false;
                promoted.push(name.clone());
            }
        }
    }
    promoted
}

pub fn generate_loader(
    merged_dir: &Path,
    scripts: &[PluginScripts],
    opts: &LoaderOptions,
) -> String {
    let mut scripts = scripts.to_vec();
    promote_lazy_to_eager(&mut scripts);

    // lazy→lazy deps: 各 lazy plugin の depends にある lazy plugin を先にロードする
    // ための依存マップを作る (phase 7 の trigger 生成で使う)
    let lazy_names: std::collections::HashSet<String> = scripts
        .iter()
        .filter(|s| s.lazy)
        .map(|s| s.name.clone())
        .collect();
    let lazy_deps_map: std::collections::HashMap<String, Vec<String>> = scripts
        .iter()
        .filter(|s| s.lazy)
        .filter_map(|s| {
            let deps: Vec<String> = s
                .depends
                .iter()
                .flatten()
                .filter(|d| lazy_names.contains(*d))
                .cloned()
                .collect();
            if deps.is_empty() {
                None
            } else {
                Some((s.name.clone(), deps))
            }
        })
        .collect();

    let mut lua = String::new();
    lua.push_str("-- rvpm generated loader.lua\n\n");

    // ======================================================
    // Neovim の auto-source を無効化 (lazy.nvim 方式)
    // これにより二重 source を防ぎ、rvpm が全ロード順序を制御する
    // ======================================================
    lua.push_str("vim.go.loadplugins = false\n\n");

    // ======================================================
    // load_lazy helper — lazy プラグインの実行時ローダー
    // 事前 glob 済みファイルリストを受け取り、ftdetect を augroup で wrap
    // ======================================================
    lua.push_str(r#"local function load_lazy(name, path, plugin_files, ftdetect_files, after_plugin_files, before, after)
  if _G["rvpm_loaded_" .. name] then return end
  _G["rvpm_loaded_" .. name] = true
  vim.opt.rtp:append(path)
  if before then dofile(before) end
  for _, f in ipairs(plugin_files) do vim.cmd("source " .. f) end
  if #ftdetect_files > 0 then
    vim.cmd("augroup filetypedetect")
    for _, f in ipairs(ftdetect_files) do vim.cmd("source " .. f) end
    vim.cmd("augroup END")
  end
  for _, f in ipairs(after_plugin_files) do vim.cmd("source " .. f) end
  if after then dofile(after) end
  vim.api.nvim_exec_autocmds("User", { pattern = "rvpm_loaded_" .. name })
end

"#);

    // ======================================================
    // グローバル before.lua (全プラグインの前)
    // leader / vim options / 基本設定を書く場所
    // ======================================================
    if let Some(before) = &opts.global_before {
        lua.push_str(&format!("dofile(\"{}\")\n\n", before.replace('\\', "/")));
    }

    // ======================================================
    // 全プラグインの init.lua (依存順)
    // init は "pre-rtp" phase であり、全プラグイン共通
    // ======================================================
    for s in &scripts {
        if let Some(init) = &s.init {
            let body = format!("dofile(\"{}\")\n", init.replace('\\', "/"));
            push_with_cond(&mut lua, &s.cond, &body);
        }
    }
    lua.push('\n');

    // ======================================================
    // merged rtp append (merge=true プラグインがあれば 1 回)
    // ======================================================
    if scripts.iter().any(|s| s.merge) {
        let merged_path = merged_dir.to_string_lossy().replace('\\', "/");
        lua.push_str(&format!("vim.opt.rtp:append(\"{}\")\n\n", merged_path));
    }

    // ======================================================
    // eager プラグイン処理 (依存順)
    // 非 merge: rtp 追加 → before → plugin/ → ftdetect/ → after/plugin/ → after
    // merge   : before → plugin/ → ftdetect/ → after/plugin/ → after
    // 事前 glob 済みのファイルを直接 source する (起動時 glob 不要)
    // ======================================================
    for s in &scripts {
        if s.lazy {
            continue;
        }
        let mut body = String::new();
        let path = s.path.replace('\\', "/");

        // 非 merge な eager プラグインは個別に rtp に追加
        if !s.merge {
            body.push_str(&format!("vim.opt.rtp:append(\"{}\")\n", path));
        }

        // before
        if let Some(before) = &s.before {
            body.push_str(&format!("dofile(\"{}\")\n", before.replace('\\', "/")));
        }

        // plugin/**/*.{vim,lua} を直接 source
        for f in &s.plugin_files {
            body.push_str(&format!("vim.cmd(\"source {}\")\n", f.replace('\\', "/")));
        }

        // ftdetect/ は filetypedetect augroup で wrap
        if !s.ftdetect_files.is_empty() {
            body.push_str("vim.cmd(\"augroup filetypedetect\")\n");
            for f in &s.ftdetect_files {
                body.push_str(&format!("vim.cmd(\"source {}\")\n", f.replace('\\', "/")));
            }
            body.push_str("vim.cmd(\"augroup END\")\n");
        }

        // after/plugin/
        for f in &s.after_plugin_files {
            body.push_str(&format!("vim.cmd(\"source {}\")\n", f.replace('\\', "/")));
        }

        // after.lua (plugin/ source 後)
        if let Some(after) = &s.after {
            body.push_str(&format!("dofile(\"{}\")\n", after.replace('\\', "/")));
        }

        // User autocmd を発火 (on_source チェーンのため)
        body.push_str(&format!(
            "vim.api.nvim_exec_autocmds(\"User\", {{ pattern = \"rvpm_loaded_{}\" }})\n",
            s.name
        ));

        push_with_cond(&mut lua, &s.cond, &body);
    }
    lua.push('\n');

    // ======================================================
    // lazy trigger 登録
    // 各プラグインの plugin/ ftdetect/ after/plugin ファイルリストを
    // ローカル変数として emit し、trigger closure から参照する
    // ======================================================
    for s in &scripts {
        if !s.lazy {
            continue;
        }
        let path = s.path.replace('\\', "/");
        let before = s
            .before
            .as_ref()
            .map(|p| format!("\"{}\"", p.replace('\\', "/")))
            .unwrap_or_else(|| "nil".to_string());
        let after = s
            .after
            .as_ref()
            .map(|p| format!("\"{}\"", p.replace('\\', "/")))
            .unwrap_or_else(|| "nil".to_string());
        let safe = sanitize_name(&s.name);
        let pf_var = format!("_rvpm_pf_{}", safe);
        let fd_var = format!("_rvpm_fd_{}", safe);
        let ap_var = format!("_rvpm_ap_{}", safe);

        let mut body = String::new();
        // do...end ブロックで local 変数をスコープ化 (Lua の 200 ローカル変数制限回避)
        body.push_str("do\n");
        // ファイルリストをローカルテーブルとして宣言
        body.push_str(&format!(
            "local {} = {}\n",
            pf_var,
            lua_str_list(&s.plugin_files)
        ));
        body.push_str(&format!(
            "local {} = {}\n",
            fd_var,
            lua_str_list(&s.ftdetect_files)
        ));
        body.push_str(&format!(
            "local {} = {}\n",
            ap_var,
            lua_str_list(&s.after_plugin_files)
        ));

        // deps がある場合は load_lazy の前に依存先をロードするコードを生成
        // 依存先のファイルリスト変数も current plugin の body 内で宣言する
        let mut deps_load = String::new();
        if let Some(deps) = lazy_deps_map.get(&s.name) {
            for dep in deps {
                if let Some(dep_script) = scripts.iter().find(|ds| ds.name == *dep) {
                    let dp = dep_script.path.replace('\\', "/");
                    let db = dep_script
                        .before
                        .as_ref()
                        .map(|p| format!("\"{}\"", p.replace('\\', "/")))
                        .unwrap_or_else(|| "nil".to_string());
                    let da = dep_script
                        .after
                        .as_ref()
                        .map(|p| format!("\"{}\"", p.replace('\\', "/")))
                        .unwrap_or_else(|| "nil".to_string());
                    let dsafe = sanitize_name(dep);
                    // 依存先のファイルリスト変数を宣言 (重複宣言は load_lazy の guard で安全)
                    body.push_str(&format!(
                        "local _rvpm_pf_{dsafe} = {}\n",
                        lua_str_list(&dep_script.plugin_files)
                    ));
                    body.push_str(&format!(
                        "local _rvpm_fd_{dsafe} = {}\n",
                        lua_str_list(&dep_script.ftdetect_files)
                    ));
                    body.push_str(&format!(
                        "local _rvpm_ap_{dsafe} = {}\n",
                        lua_str_list(&dep_script.after_plugin_files)
                    ));
                    deps_load.push_str(&format!(
                        "load_lazy(\"{dep}\", \"{dp}\", _rvpm_pf_{dsafe}, _rvpm_fd_{dsafe}, _rvpm_ap_{dsafe}, {db}, {da})\n  ",
                    ));
                }
            }
        }

        let load_call = format!(
            "{deps_load}load_lazy(\"{}\", \"{}\", {}, {}, {}, {}, {})",
            s.name, path, pf_var, fd_var, ap_var, before, after
        );

        // ---- on_cmd: lazy.nvim 方式 ----
        // bang/range/count/mods/args を event から復元して vim.cmd(table) で dispatch
        // complete callback は plugin をロードしてから vim.fn.getcompletion に委譲
        if let Some(cmds) = &s.on_cmd {
            for cmd in cmds {
                body.push_str(&format!(
                    "vim.api.nvim_create_user_command(\"{cmd}\", function(event)\n\
                     \x20 pcall(vim.api.nvim_del_user_command, \"{cmd}\")\n\
                     \x20 {load}\n\
                     \x20 local cmd = {{ cmd = \"{cmd}\", bang = event.bang or nil, mods = event.smods, args = event.fargs }}\n\
                     \x20 if event.range == 1 then\n\
                     \x20   cmd.range = {{ event.line1 }}\n\
                     \x20 elseif event.range == 2 then\n\
                     \x20   cmd.range = {{ event.line1, event.line2 }}\n\
                     \x20 end\n\
                     \x20 if event.count >= 0 and event.range == 0 then\n\
                     \x20   cmd.count = event.count\n\
                     \x20 end\n\
                     \x20 vim.cmd(cmd)\n\
                     end, {{\n\
                     \x20 bang = true,\n\
                     \x20 range = true,\n\
                     \x20 nargs = \"*\",\n\
                     \x20 complete = function(_, line)\n\
                     \x20   pcall(vim.api.nvim_del_user_command, \"{cmd}\")\n\
                     \x20   {load}\n\
                     \x20   return vim.fn.getcompletion(line, \"cmdline\")\n\
                     \x20 end,\n\
                     }})\n",
                    cmd = cmd,
                    load = load_call,
                ));
            }
        }

        // ---- on_ft: FileType を再トリガーして ftplugin/ を発火 ----
        if let Some(fts) = &s.on_ft {
            body.push_str(&format!(
                "vim.api.nvim_create_autocmd(\"FileType\", {{ pattern = {{ \"{}\" }}, once = true, callback = function(ev)\n\
                 \x20 {load}\n\
                 \x20 vim.api.nvim_exec_autocmds(\"FileType\", {{ buffer = ev.buf, modeline = false }})\n\
                 end }})\n",
                fts.join("\", \""),
                load = load_call,
            ));
        }

        // ---- on_map: lhs + mode (+ desc) 対応、<Ignore> prefix で安全に replay ----
        if let Some(maps) = &s.on_map {
            for m in maps {
                let modes = m.modes_or_default();
                let modes_lua = lua_str_list(&modes);
                let lhs = &m.lhs;
                let opts_table = match &m.desc {
                    Some(d) => format!(", {{ desc = \"{}\" }}", d.replace('"', "\\\"")),
                    None => String::new(),
                };
                body.push_str(&format!(
                    "vim.keymap.set({modes}, \"{lhs}\", function()\n\
                     \x20 vim.keymap.del({modes}, \"{lhs}\")\n\
                     \x20 {load}\n\
                     \x20 local feed = vim.api.nvim_replace_termcodes(\"<Ignore>{lhs}\", true, true, true)\n\
                     \x20 vim.api.nvim_feedkeys(feed, \"m\", false)\n\
                     end{opts})\n",
                    modes = modes_lua,
                    lhs = lhs,
                    load = load_call,
                    opts = opts_table,
                ));
            }
        }

        // ---- on_event: ロード後に event を再発火 (buffer + data 保持) ----
        // "User Xxx" 形式は User autocmd + pattern="Xxx" として切り出し、
        // それ以外のイベントはまとめて 1 つの autocmd にする
        if let Some(events) = &s.on_event {
            let mut regular: Vec<String> = Vec::new();
            let mut user_patterns: Vec<String> = Vec::new();
            for e in events {
                if let Some(pat) = e.strip_prefix("User ") {
                    user_patterns.push(pat.trim().to_string());
                } else {
                    regular.push(e.clone());
                }
            }

            if !regular.is_empty() {
                body.push_str(&format!(
                    "vim.api.nvim_create_autocmd({{ \"{}\" }}, {{ once = true, callback = function(ev)\n\
                     \x20 {load}\n\
                     \x20 vim.api.nvim_exec_autocmds(ev.event, {{ buffer = ev.buf, data = ev.data, modeline = false }})\n\
                     end }})\n",
                    regular.join("\", \""),
                    load = load_call,
                ));
            }

            for pat in &user_patterns {
                body.push_str(&format!(
                    "vim.api.nvim_create_autocmd(\"User\", {{ pattern = \"{pat}\", once = true, callback = function(ev)\n\
                     \x20 {load}\n\
                     \x20 vim.api.nvim_exec_autocmds(\"User\", {{ pattern = \"{pat}\", data = ev.data, modeline = false }})\n\
                     end }})\n",
                    pat = pat,
                    load = load_call,
                ));
            }
        }

        // ---- on_path: BufRead/BufNewFile 再発火で buffer 状態を復元 ----
        if let Some(paths) = &s.on_path {
            body.push_str(&format!(
                "vim.api.nvim_create_autocmd({{ \"BufRead\", \"BufNewFile\" }}, {{ pattern = {{ \"{}\" }}, once = true, callback = function(ev)\n\
                 \x20 {load}\n\
                 \x20 vim.api.nvim_exec_autocmds(ev.event, {{ buffer = ev.buf, data = ev.data, modeline = false }})\n\
                 end }})\n",
                paths.join("\", \""),
                load = load_call,
            ));
        }

        // ---- on_source: プラグインロード完了 User イベントを受けて連鎖 ----
        if let Some(sources) = &s.on_source {
            let patterns: Vec<String> = sources
                .iter()
                .map(|src| format!("rvpm_loaded_{}", src))
                .collect();
            body.push_str(&format!(
                "vim.api.nvim_create_autocmd(\"User\", {{ pattern = {{ \"{}\" }}, once = true, callback = function()\n\
                 \x20 {load}\n\
                 end }})\n",
                patterns.join("\", \""),
                load = load_call,
            ));
        }

        body.push_str("end\n");
        push_with_cond(&mut lua, &s.cond, &body);
    }

    // ======================================================
    // ColorSchemePre handler (lazy colorscheme 自動ロード)
    // lazy plugin の colors/ に含まれるカラースキーム名をマップ化し、
    // `:colorscheme <name>` 実行時に対応プラグインをロードする
    // ======================================================
    {
        // colorscheme → plugin の load_lazy 呼び出しコードを集める
        let mut cs_entries: Vec<String> = Vec::new();
        for s in &scripts {
            if !s.lazy || s.colorschemes.is_empty() {
                continue;
            }
            let path = s.path.replace('\\', "/");
            let before = s
                .before
                .as_ref()
                .map(|p| format!("\"{}\"", p.replace('\\', "/")))
                .unwrap_or_else(|| "nil".to_string());
            let after = s
                .after
                .as_ref()
                .map(|p| format!("\"{}\"", p.replace('\\', "/")))
                .unwrap_or_else(|| "nil".to_string());
            // ファイルリストをインライン展開 (do...end スコープ外のため変数参照不可)
            let pf_inline = lua_str_list(&s.plugin_files);
            let fd_inline = lua_str_list(&s.ftdetect_files);
            let ap_inline = lua_str_list(&s.after_plugin_files);
            for cs in &s.colorschemes {
                cs_entries.push(format!(
                    "[\"{cs}\"] = function() load_lazy(\"{name}\", \"{path}\", {pf}, {fd}, {ap}, {before}, {after}) end",
                    cs = cs,
                    name = s.name,
                    path = path,
                    pf = pf_inline,
                    fd = fd_inline,
                    ap = ap_inline,
                    before = before,
                    after = after,
                ));
            }
        }

        if !cs_entries.is_empty() {
            lua.push_str("local _rvpm_colorschemes = {\n");
            for entry in &cs_entries {
                lua.push_str(&format!("  {},\n", entry));
            }
            lua.push_str("}\n");
            lua.push_str(
                "vim.api.nvim_create_autocmd(\"ColorSchemePre\", {\n\
                 \x20 callback = function(ev)\n\
                 \x20   local loader = _rvpm_colorschemes[ev.match]\n\
                 \x20   if loader then loader() end\n\
                 \x20 end,\n\
                 })\n\n",
            );
        }
    }

    // ======================================================
    // グローバル after.lua (全プラグインの後)
    // colorscheme / 最終 UI 調整を書く場所
    // ======================================================
    if let Some(after) = &opts.global_after {
        lua.push_str(&format!("\ndofile(\"{}\")\n", after.replace('\\', "/")));
    }

    lua
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================
    // 新モデル: lazy.nvim 方式 + merge optimization + 事前コンパイル
    // ========================================================

    // ========================================================
    // depends テスト
    // ========================================================

    #[test]
    fn test_eager_depending_on_lazy_promotes_to_eager() {
        // Lazy A に Eager B が depends → A は eager に昇格して phase 6 で B より先にロード
        let mut a = PluginScripts::for_test("snacks.nvim", "/path/snacks");
        a.lazy = true;
        a.on_cmd = Some(vec!["Snacks".to_string()]);
        a.plugin_files = vec!["/path/snacks/plugin/snacks.lua".to_string()];

        let mut b = PluginScripts::for_test("telescope.nvim", "/path/telescope");
        b.lazy = false;
        b.depends = Some(vec!["snacks.nvim".to_string()]);
        b.plugin_files = vec!["/path/telescope/plugin/telescope.lua".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[a, b]);
        // A が phase 6 (eager) で source されている (lazy trigger ではない)
        let snacks_source = lua
            .find("source /path/snacks/plugin/snacks.lua")
            .expect("snacks should be sourced eagerly");
        let telescope_source = lua
            .find("source /path/telescope/plugin/telescope.lua")
            .expect("telescope should be sourced");
        assert!(
            snacks_source < telescope_source,
            "snacks (promoted eager) must load before telescope"
        );
        // A の on_cmd trigger は登録されない (eager になったので不要)
        assert!(
            !lua.contains("nvim_create_user_command(\"Snacks\""),
            "promoted plugin should not register lazy triggers"
        );
    }

    #[test]
    fn test_on_source_referencing_eager_promotes_to_eager() {
        // Lazy A の on_source が Eager B を参照 → A は eager に昇格
        // (phase 6 で B の rvpm_loaded 発火時に phase 7 のリスナーが未登録の問題を回避)
        let mut b = PluginScripts::for_test("snacks.nvim", "/path/snacks");
        b.lazy = false;
        b.plugin_files = vec!["/path/snacks/plugin/snacks.lua".to_string()];

        let mut a = PluginScripts::for_test("telescope.nvim", "/path/telescope");
        a.lazy = true;
        a.on_source = Some(vec!["snacks.nvim".to_string()]);
        a.plugin_files = vec!["/path/telescope/plugin/telescope.lua".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[b, a]);
        // A が phase 6 (eager) で source されている
        assert!(
            lua.contains("source /path/telescope/plugin/telescope.lua"),
            "on_source→eager plugin should be promoted and sourced eagerly"
        );
        // A の on_source trigger は登録されていない
        assert!(
            !lua.contains("rvpm_loaded_snacks.nvim\", once = true"),
            "promoted plugin should not register on_source trigger"
        );
    }

    #[test]
    fn test_on_source_chain_promotion() {
        // A(eager) ← B(lazy, on_source=["A"]) ← C(lazy, on_source=["B"])
        // → B 昇格 → C も昇格
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = false;
        a.plugin_files = vec!["/path/a/plugin/a.lua".to_string()];

        let mut b = PluginScripts::for_test("b", "/path/b");
        b.lazy = true;
        b.on_source = Some(vec!["a".to_string()]);
        b.plugin_files = vec!["/path/b/plugin/b.lua".to_string()];

        let mut c = PluginScripts::for_test("c", "/path/c");
        c.lazy = true;
        c.on_source = Some(vec!["b".to_string()]);
        c.plugin_files = vec!["/path/c/plugin/c.lua".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[a, b, c]);
        // 全部 eager で source されている
        assert!(lua.contains("source /path/a/plugin/a.lua"));
        assert!(lua.contains("source /path/b/plugin/b.lua"));
        assert!(lua.contains("source /path/c/plugin/c.lua"));
        // source 順序: a → b → c
        let pos_a = lua.find("source /path/a/plugin/a.lua").unwrap();
        let pos_b = lua.find("source /path/b/plugin/b.lua").unwrap();
        let pos_c = lua.find("source /path/c/plugin/c.lua").unwrap();
        assert!(pos_a < pos_b, "a must load before b");
        assert!(pos_b < pos_c, "b must load before c");
    }

    #[test]
    fn test_lazy_depending_on_lazy_loads_deps_first() {
        // Lazy A に Lazy B が depends → B の trigger 発火時に A を先にロード
        let mut a = PluginScripts::for_test("snacks.nvim", "/path/snacks");
        a.lazy = true;
        a.plugin_files = vec!["/path/snacks/plugin/snacks.lua".to_string()];

        let mut b = PluginScripts::for_test("telescope.nvim", "/path/telescope");
        b.lazy = true;
        b.on_cmd = Some(vec!["Telescope".to_string()]);
        b.depends = Some(vec!["snacks.nvim".to_string()]);
        b.plugin_files = vec!["/path/telescope/plugin/telescope.lua".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[a, b]);
        // B の trigger 内で A の load_lazy が B の前に呼ばれる
        let trigger_section = lua
            .find("nvim_create_user_command(\"Telescope\"")
            .expect("telescope trigger missing");
        let after_trigger = &lua[trigger_section..];
        // trigger callback 内に snacks の load_lazy 呼び出しがある
        assert!(
            after_trigger.contains("load_lazy(\"snacks.nvim\""),
            "telescope trigger should load snacks.nvim dependency first:\n{}",
            &after_trigger[..500.min(after_trigger.len())]
        );
    }

    #[test]
    fn test_mixed_depends_and_on_source() {
        // A(eager, depends=["B"]) が B(lazy) に依存 → B 昇格
        // C(lazy, on_source=["B"]) が B を参照 → B は now eager → C 昇格
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = false;
        a.depends = Some(vec!["b".to_string()]);
        a.plugin_files = vec!["/path/a/plugin/a.lua".to_string()];

        let mut b = PluginScripts::for_test("b", "/path/b");
        b.lazy = true;
        b.plugin_files = vec!["/path/b/plugin/b.lua".to_string()];

        let mut c = PluginScripts::for_test("c", "/path/c");
        c.lazy = true;
        c.on_source = Some(vec!["b".to_string()]);
        c.plugin_files = vec!["/path/c/plugin/c.lua".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[b, a, c]);
        // 全部 eager に昇格して source
        assert!(lua.contains("source /path/a/plugin/a.lua"));
        assert!(lua.contains("source /path/b/plugin/b.lua"));
        assert!(lua.contains("source /path/c/plugin/c.lua"));
        let pb = lua.find("source /path/b/plugin/b.lua").unwrap();
        let pa = lua.find("source /path/a/plugin/a.lua").unwrap();
        let pc = lua.find("source /path/c/plugin/c.lua").unwrap();
        assert!(pb < pa, "b must load before a (a depends on b)");
        assert!(pb < pc, "b must load before c (c on_source b)");
    }

    #[test]
    fn test_circular_on_source_does_not_infinite_loop() {
        // A(lazy, on_source=["B"]) + B(lazy, on_source=["A"])
        // 両方 lazy で互いに on_source → eager 昇格は起きない (eager がないので)
        // ループせずに収束すること
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = true;
        a.on_source = Some(vec!["b".to_string()]);

        let mut b = PluginScripts::for_test("b", "/path/b");
        b.lazy = true;
        b.on_source = Some(vec!["a".to_string()]);

        // パニックしなければ OK (無限ループしない)
        let lua = gen_loader(Path::new("/merged"), &[a, b]);
        // 両方 lazy のまま (eager 昇格は起きない)
        assert!(!lua.contains("source /path/a/plugin"), "a should stay lazy");
        assert!(!lua.contains("source /path/b/plugin"), "b should stay lazy");
    }

    #[test]
    fn test_circular_depends_does_not_infinite_loop() {
        // A(lazy, depends=["B"]) + B(lazy, depends=["A"]) — 相互依存
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = true;
        a.depends = Some(vec!["b".to_string()]);
        a.on_cmd = Some(vec!["FooA".to_string()]);

        let mut b = PluginScripts::for_test("b", "/path/b");
        b.lazy = true;
        b.depends = Some(vec!["a".to_string()]);
        b.on_cmd = Some(vec!["FooB".to_string()]);

        // パニック・無限ループしなければ OK
        let lua = gen_loader(Path::new("/merged"), &[a, b]);
        // 両方 lazy のまま (eager がないので昇格しない)
        assert!(
            lua.contains("nvim_create_user_command(\"FooA\""),
            "a trigger exists"
        );
        assert!(
            lua.contains("nvim_create_user_command(\"FooB\""),
            "b trigger exists"
        );
    }

    #[test]
    fn test_circular_depends_with_eager_involved() {
        // A(eager, depends=["B"]) + B(lazy, depends=["A"]) — A が eager で B に依存、B が A に逆依存
        // → B は A に依存されるので eager に昇格
        // → 昇格後の B が A に depends しているが A も eager → 無限昇格ループにならない
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = false;
        a.depends = Some(vec!["b".to_string()]);
        a.plugin_files = vec!["/path/a/plugin/a.lua".to_string()];

        let mut b = PluginScripts::for_test("b", "/path/b");
        b.lazy = true;
        b.depends = Some(vec!["a".to_string()]);
        b.plugin_files = vec!["/path/b/plugin/b.lua".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[a, b]);
        // B は eager に昇格されて source
        assert!(
            lua.contains("source /path/b/plugin/b.lua"),
            "b should be promoted"
        );
        assert!(
            lua.contains("source /path/a/plugin/a.lua"),
            "a should be sourced"
        );
    }

    #[test]
    fn test_three_way_circular_depends() {
        // A→B→C→A の循環 (全 lazy)
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = true;
        a.depends = Some(vec!["c".to_string()]);
        a.on_cmd = Some(vec!["FooA".to_string()]);

        let mut b = PluginScripts::for_test("b", "/path/b");
        b.lazy = true;
        b.depends = Some(vec!["a".to_string()]);
        b.on_cmd = Some(vec!["FooB".to_string()]);

        let mut c = PluginScripts::for_test("c", "/path/c");
        c.lazy = true;
        c.depends = Some(vec!["b".to_string()]);
        c.on_cmd = Some(vec!["FooC".to_string()]);

        // 無限ループしない
        let lua = gen_loader(Path::new("/merged"), &[a, b, c]);
        // 全部 lazy のまま
        assert!(lua.contains("nvim_create_user_command(\"FooA\""));
        assert!(lua.contains("nvim_create_user_command(\"FooB\""));
        assert!(lua.contains("nvim_create_user_command(\"FooC\""));
    }

    #[test]
    fn test_self_referential_depends_does_not_crash() {
        // A(lazy, depends=["A"]) — 自己参照
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = true;
        a.depends = Some(vec!["a".to_string()]);
        // パニックしなければ OK
        let _lua = gen_loader(Path::new("/merged"), &[a]);
    }

    #[test]
    fn test_eager_with_on_source_is_harmless() {
        // Eager plugin が on_source を持っている (設定ミス)
        // → 無視される (on_source は phase 7 で lazy のみ処理)
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = false;
        a.on_source = Some(vec!["nonexistent".to_string()]);
        a.plugin_files = vec!["/path/a/plugin/a.lua".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[a]);
        // phase 6 で正常に source される
        assert!(lua.contains("source /path/a/plugin/a.lua"));
        // on_source trigger は phase 7 に出ない (eager なので skip)
        assert!(!lua.contains("rvpm_loaded_nonexistent"));
    }

    #[test]
    fn test_reverse_depends_on_source_combo() {
        // A(lazy, on_cmd=["FooA"]) → B(lazy, depends=["A"], on_source=["A"])
        // B は depends でも on_source でも A を参照。A は lazy のまま。
        // B のトリガー発火時に A が先にロードされる (depends chain)。
        // on_source はさらに A の rvpm_loaded でも B をロードする (二重ガードで安全)。
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = true;
        a.on_cmd = Some(vec!["FooA".to_string()]);
        a.plugin_files = vec!["/path/a/plugin/a.lua".to_string()];

        let mut b = PluginScripts::for_test("b", "/path/b");
        b.lazy = true;
        b.depends = Some(vec!["a".to_string()]);
        b.on_source = Some(vec!["a".to_string()]);
        b.plugin_files = vec!["/path/b/plugin/b.lua".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[a, b]);
        // 両方 lazy のまま (eager 参照がない)
        // B の trigger 内で A が先にロードされる
        assert!(
            lua.contains("nvim_create_user_command(\"FooA\""),
            "A trigger exists"
        );
        assert!(
            lua.contains("rvpm_loaded_a"),
            "B on_source trigger for A exists"
        );
    }

    // ========================================================
    // colorscheme 自動検出テスト
    // ========================================================

    #[test]
    fn test_lazy_plugin_with_colorschemes_emits_colorscheme_pre_handler() {
        let mut s = PluginScripts::for_test("catppuccin", "/path/catppuccin");
        s.lazy = true;
        s.colorschemes = vec!["catppuccin".to_string(), "catppuccin-latte".to_string()];
        s.plugin_files = vec!["/path/catppuccin/plugin/catppuccin.lua".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[s]);
        assert!(
            lua.contains("ColorSchemePre"),
            "should register ColorSchemePre autocmd for lazy colorscheme"
        );
        assert!(lua.contains("catppuccin"), "should reference catppuccin");
        assert!(
            lua.contains("catppuccin-latte"),
            "should reference catppuccin-latte"
        );
    }

    #[test]
    fn test_eager_plugin_with_colorschemes_no_handler() {
        let mut s = PluginScripts::for_test("catppuccin", "/path/catppuccin");
        s.lazy = false;
        s.colorschemes = vec!["catppuccin".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[s]);
        assert!(
            !lua.contains("ColorSchemePre"),
            "eager plugin should NOT register ColorSchemePre handler"
        );
    }

    #[test]
    fn test_multiple_lazy_colorscheme_plugins_combined_handler() {
        let mut a = PluginScripts::for_test("catppuccin", "/path/catppuccin");
        a.lazy = true;
        a.colorschemes = vec!["catppuccin".to_string()];
        a.plugin_files = vec!["/path/catppuccin/plugin/c.lua".to_string()];

        let mut b = PluginScripts::for_test("tokyonight", "/path/tokyonight");
        b.lazy = true;
        b.colorschemes = vec!["tokyonight".to_string(), "tokyonight-night".to_string()];
        b.plugin_files = vec!["/path/tokyonight/plugin/t.lua".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[a, b]);
        assert!(lua.contains("catppuccin"));
        assert!(lua.contains("tokyonight"));
        assert!(lua.contains("tokyonight-night"));
    }

    #[test]
    fn test_colorscheme_handler_loads_correct_plugin() {
        let mut s = PluginScripts::for_test("catppuccin", "/path/catppuccin");
        s.lazy = true;
        s.colorschemes = vec!["catppuccin".to_string()];
        s.plugin_files = vec!["/path/catppuccin/plugin/c.lua".to_string()];

        let lua = gen_loader(Path::new("/merged"), &[s]);
        assert!(
            lua.contains("load_lazy(\"catppuccin\""),
            "ColorSchemePre handler should call load_lazy for the matching plugin"
        );
    }

    // ========================================================
    // グローバル hooks テスト
    // ========================================================

    #[test]
    fn test_loader_global_before_runs_before_all_plugins() {
        let mut s = PluginScripts::for_test("a", "/path/a");
        s.init = Some("/cfg/a/init.lua".to_string());
        let opts = LoaderOptions {
            global_before: Some("/rvpm/before.lua".to_string()),
            global_after: None,
        };
        let lua = generate_loader(Path::new("/merged"), &[s], &opts);
        let before_pos = lua.find("/rvpm/before.lua").expect("global before missing");
        let init_pos = lua.find("/cfg/a/init.lua").expect("plugin init missing");
        assert!(
            before_pos < init_pos,
            "global before must run BEFORE any plugin init"
        );
    }

    #[test]
    fn test_loader_global_after_runs_after_all_lazy_triggers() {
        let mut s = PluginScripts::for_test("a", "/path/a");
        s.lazy = true;
        s.on_cmd = Some(vec!["Foo".to_string()]);
        let opts = LoaderOptions {
            global_before: None,
            global_after: Some("/rvpm/after.lua".to_string()),
        };
        let lua = generate_loader(Path::new("/merged"), &[s], &opts);
        let trigger_pos = lua
            .find("nvim_create_user_command")
            .expect("trigger missing");
        let after_pos = lua.find("/rvpm/after.lua").expect("global after missing");
        assert!(
            trigger_pos < after_pos,
            "global after must run AFTER lazy trigger registrations"
        );
    }

    #[test]
    fn test_loader_no_global_hooks_when_none() {
        let opts = LoaderOptions {
            global_before: None,
            global_after: None,
        };
        let lua = generate_loader(Path::new("/merged"), &[], &opts);
        // global hooks のセクションコメントがあっても dofile は出ない
        assert!(
            !lua.contains("dofile") || lua.contains("load_lazy"),
            "no dofile for global hooks when None"
        );
    }

    // ========================================================
    // 新モデルテスト
    // ========================================================

    #[test]
    fn test_loader_disables_neovim_plugin_loading() {
        let lua = gen_loader(Path::new("/merged"), &[]);
        assert!(
            lua.contains("vim.go.loadplugins = false"),
            "loader must disable Neovim's default plugin loading"
        );
    }

    #[test]
    fn test_loader_phase_order_init_rtp_before() {
        let mut s = PluginScripts::for_test("a", "/path/a");
        s.merge = true;
        s.init = Some("/cfg/a/init.lua".to_string());
        s.before = Some("/cfg/a/before.lua".to_string());
        let lua = gen_loader(Path::new("/merged"), &[s]);
        let init_pos = lua.find("/cfg/a/init.lua").expect("init missing");
        let rtp_pos = lua
            .find("vim.opt.rtp:append(\"/merged\")")
            .expect("merged rtp missing");
        let before_pos = lua.find("/cfg/a/before.lua").expect("before missing");
        assert!(
            init_pos < rtp_pos,
            "init must come BEFORE merged rtp append"
        );
        assert!(rtp_pos < before_pos, "before must come AFTER rtp append");
    }

    #[test]
    fn test_loader_merged_rtp_appended_exactly_once() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.merge = true;
        let mut b = PluginScripts::for_test("b", "/path/b");
        b.merge = true;
        let lua = gen_loader(Path::new("/merged"), &[a, b]);
        let count = lua.matches("vim.opt.rtp:append(\"/merged\")").count();
        assert_eq!(
            count, 1,
            "merged rtp should be appended exactly once for multiple merge=true plugins"
        );
    }

    #[test]
    fn test_loader_no_merged_rtp_when_all_non_merge() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.merge = false;
        let lua = gen_loader(Path::new("/merged"), &[a]);
        assert!(
            !lua.contains("vim.opt.rtp:append(\"/merged\")"),
            "should NOT append merged rtp when no merge=true plugin exists"
        );
    }

    #[test]
    fn test_loader_non_merge_eager_appends_own_rtp() {
        let mut a = PluginScripts::for_test("solo", "/path/solo");
        a.merge = false;
        let lua = gen_loader(Path::new("/merged"), &[a]);
        assert!(
            lua.contains("vim.opt.rtp:append(\"/path/solo\")"),
            "non-merge eager plugin must append its own path to rtp"
        );
    }

    #[test]
    fn test_loader_eager_sources_plugin_files_between_before_and_after() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.merge = true;
        a.before = Some("/cfg/a/before.lua".to_string());
        a.after = Some("/cfg/a/after.lua".to_string());
        a.plugin_files = vec!["/path/a/plugin/a.vim".to_string()];
        let lua = gen_loader(Path::new("/merged"), &[a]);
        let before_pos = lua.find("/cfg/a/before.lua").unwrap();
        let source_pos = lua
            .find("vim.cmd(\"source /path/a/plugin/a.vim\")")
            .expect("plugin file source missing");
        let after_pos = lua.find("/cfg/a/after.lua").unwrap();
        assert!(
            before_pos < source_pos,
            "before.lua must come before plugin/ source"
        );
        assert!(
            source_pos < after_pos,
            "after.lua must come after plugin/ source"
        );
    }

    #[test]
    fn test_loader_eager_wraps_ftdetect_in_filetypedetect_augroup() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.merge = true;
        a.ftdetect_files = vec!["/path/a/ftdetect/a.vim".to_string()];
        let lua = gen_loader(Path::new("/merged"), &[a]);
        // eager phase 内の ftdetect source を探し、その直前/直後に augroup begin/end があるか確認
        // (load_lazy helper 内の augroup とは別に、phase 6 の augroup が必要)
        let ftdetect_source_pos = lua
            .find("vim.cmd(\"source /path/a/ftdetect/a.vim\")")
            .expect("ftdetect source missing");
        // source の手前に "augroup filetypedetect" (source より前の範囲で rfind)
        let prior = &lua[..ftdetect_source_pos];
        let augroup_begin_pos = prior
            .rfind("augroup filetypedetect")
            .expect("augroup begin missing before ftdetect source");
        // source の後ろに "augroup END"
        let after = &lua[ftdetect_source_pos..];
        let augroup_end_rel = after
            .find("augroup END")
            .expect("augroup END missing after ftdetect source");
        // source と begin/end の間に他の augroup END/begin がないことも軽く確認
        assert!(augroup_begin_pos < ftdetect_source_pos);
        assert!(augroup_end_rel > 0);
    }

    #[test]
    fn test_loader_eager_sources_after_plugin_files() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.merge = true;
        a.after_plugin_files = vec!["/path/a/after/plugin/a.vim".to_string()];
        let lua = gen_loader(Path::new("/merged"), &[a]);
        assert!(
            lua.contains("vim.cmd(\"source /path/a/after/plugin/a.vim\")"),
            "after/plugin files must be sourced"
        );
    }

    #[test]
    fn test_loader_no_plugin_files_emitted_for_lazy_plugin() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = true;
        a.merge = false;
        a.on_cmd = Some(vec!["Foo".to_string()]);
        a.plugin_files = vec!["/path/a/plugin/a.vim".to_string()];
        let lua = gen_loader(Path::new("/merged"), &[a]);
        // lazy plugin の plugin_files は eager の位置で直接 source されない
        // (load_lazy 経由で動的に呼ばれる)
        // eager の source とは区別して、 trigger 経由でのみ呼ばれる
        // → top-level に "vim.cmd(\"source /path/a/plugin/a.vim\")" が直接出てはいけない
        // (load_lazy の中のローカルテーブル内には出ていい)
        let direct_source_count = lua
            .lines()
            .filter(|l| {
                l.trim_start()
                    .starts_with("vim.cmd(\"source /path/a/plugin/a.vim\")")
            })
            .count();
        assert_eq!(
            direct_source_count, 0,
            "lazy plugin files must not be sourced eagerly at top level"
        );
    }

    #[test]
    fn test_loader_lazy_trigger_passes_file_lists_to_load_lazy() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = true;
        a.on_cmd = Some(vec!["Foo".to_string()]);
        a.plugin_files = vec!["/path/a/plugin/a.vim".to_string()];
        a.ftdetect_files = vec!["/path/a/ftdetect/a.vim".to_string()];
        a.after_plugin_files = vec!["/path/a/after/plugin/a.vim".to_string()];
        let lua = gen_loader(Path::new("/merged"), &[a]);
        // ファイルリストがどこかに登場すること (ローカルテーブルとしてでも load_lazy 引数内でも OK)
        assert!(
            lua.contains("/path/a/plugin/a.vim"),
            "plugin file must be referenced"
        );
        assert!(
            lua.contains("/path/a/ftdetect/a.vim"),
            "ftdetect file must be referenced"
        );
        assert!(
            lua.contains("/path/a/after/plugin/a.vim"),
            "after/plugin file must be referenced"
        );
    }

    #[test]
    fn test_load_lazy_helper_sources_ftdetect_in_augroup() {
        let lua = gen_loader(Path::new("/merged"), &[]);
        // load_lazy 関数定義の中に ftdetect の処理が augroup 付きで入っているか
        let load_lazy_start = lua
            .find("local function load_lazy")
            .expect("load_lazy definition missing");
        let end_marker = lua[load_lazy_start..]
            .find("\nend\n")
            .expect("load_lazy end missing")
            + load_lazy_start;
        let body = &lua[load_lazy_start..end_marker];
        assert!(
            body.contains("augroup filetypedetect"),
            "load_lazy must wrap ftdetect in filetypedetect augroup"
        );
        assert!(
            body.contains("augroup END"),
            "load_lazy must close the augroup"
        );
    }

    // ========================================================
    // Lazy trigger 改善テスト (lazy.nvim 参考)
    // ========================================================

    /// テスト用: デフォルト opts で generate_loader を呼ぶ
    fn gen_loader(merged: &Path, scripts: &[PluginScripts]) -> String {
        generate_loader(merged, scripts, &LoaderOptions::default())
    }

    fn make_lazy_plugin(name: &str) -> PluginScripts {
        let mut s = PluginScripts::for_test(name, &format!("/path/{}", name));
        s.lazy = true;
        s
    }

    #[test]
    fn test_on_cmd_handler_has_bang_range_complete_options() {
        let mut s = make_lazy_plugin("tel");
        s.on_cmd = Some(vec!["Telescope".to_string()]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        // user command 定義に bang/range/complete オプションが入っている
        assert!(lua.contains("bang = true"), "on_cmd must enable bang");
        assert!(lua.contains("range = true"), "on_cmd must enable range");
        assert!(
            lua.contains("complete ="),
            "on_cmd must provide complete callback"
        );
        assert!(
            lua.contains("nargs = \"*\""),
            "on_cmd still supports any args"
        );
    }

    #[test]
    fn test_on_cmd_handler_reconstructs_command_from_event() {
        let mut s = make_lazy_plugin("tel");
        s.on_cmd = Some(vec!["Telescope".to_string()]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        // callback は event から bang/mods/args を取り出して vim.cmd(table) で dispatch
        assert!(lua.contains("event.bang"), "should read event.bang");
        assert!(lua.contains("event.smods"), "should read event.smods");
        assert!(lua.contains("event.fargs"), "should read event.fargs");
        assert!(lua.contains("event.range"), "should read event.range");
        assert!(lua.contains("event.count"), "should read event.count");
        // vim.cmd に table を渡している (文字列連結ではない)
        assert!(
            lua.contains("vim.cmd(cmd)") || lua.contains("vim.cmd(_rvpm_cmd"),
            "should dispatch via vim.cmd(table), not string concatenation"
        );
    }

    #[test]
    fn test_on_cmd_handler_complete_loads_plugin_and_delegates() {
        let mut s = make_lazy_plugin("tel");
        s.on_cmd = Some(vec!["Telescope".to_string()]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        // complete callback 内で load_lazy が呼ばれ、getcompletion でデリゲート
        assert!(
            lua.contains("vim.fn.getcompletion"),
            "complete callback should delegate to vim.fn.getcompletion"
        );
    }

    #[test]
    fn test_on_ft_handler_retriggers_filetype_event_after_load() {
        let mut s = make_lazy_plugin("nvim-rust");
        s.on_ft = Some(vec!["rust".to_string()]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        // ロード後に FileType を exec_autocmds で再発火
        assert!(
            lua.contains("nvim_exec_autocmds(\"FileType\""),
            "on_ft callback must re-trigger FileType after load so ftplugin/ fires"
        );
        assert!(
            lua.contains("buffer = ev.buf"),
            "re-trigger must use the original buffer"
        );
    }

    #[test]
    fn test_on_event_handler_refires_event_with_buffer_and_data() {
        let mut s = make_lazy_plugin("lsp");
        s.on_event = Some(vec!["BufReadPre".to_string()]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        // ロード後に ev.event を buffer と data 付きで再発火
        assert!(
            lua.contains("nvim_exec_autocmds(ev.event"),
            "on_event callback must re-fire the triggering event"
        );
        assert!(lua.contains("buffer = ev.buf"));
        assert!(lua.contains("data = ev.data"));
    }

    #[test]
    fn test_on_event_user_prefix_creates_user_autocmd_with_pattern() {
        let mut s = make_lazy_plugin("lazyvim-extras");
        s.on_event = Some(vec!["User LazyVimStarted".to_string()]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        // User autocmd が pattern 指定で登録されている
        assert!(
            lua.contains("nvim_create_autocmd(\"User\""),
            "User event must create a User autocmd"
        );
        assert!(
            lua.contains("pattern = \"LazyVimStarted\"")
                || lua.contains("pattern = { \"LazyVimStarted\" }"),
            "User event must specify the pattern"
        );
    }

    #[test]
    fn test_on_event_mixes_regular_and_user_events() {
        let mut s = make_lazy_plugin("mixed");
        s.on_event = Some(vec![
            "BufReadPre".to_string(),
            "User LazyVimStarted".to_string(),
        ]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        // 通常イベントの autocmd (BufReadPre)
        assert!(
            lua.contains("BufReadPre"),
            "regular event BufReadPre must still be registered"
        );
        // User autocmd も登録されている
        assert!(
            lua.contains("nvim_create_autocmd(\"User\""),
            "User event must also be registered"
        );
        assert!(
            lua.contains("LazyVimStarted"),
            "User pattern must be present"
        );
    }

    #[test]
    fn test_on_path_handler_refires_event_after_load() {
        let mut s = make_lazy_plugin("rust-tools");
        s.on_path = Some(vec!["*.rs".to_string()]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        // BufRead/BufNewFile の再発火
        assert!(
            lua.contains("nvim_exec_autocmds(ev.event"),
            "on_path callback must re-fire the BufRead/BufNewFile event"
        );
        assert!(lua.contains("buffer = ev.buf"));
    }

    #[test]
    fn test_on_map_handler_uses_ignore_prefix_feedkeys() {
        let mut s = make_lazy_plugin("keyed");
        s.on_map = Some(vec![MapSpec {
            lhs: "<leader>f".to_string(),
            mode: Vec::new(),
            desc: None,
        }]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        // <Ignore> prefix で recursion 保護
        assert!(
            lua.contains("<Ignore>"),
            "on_map replay must use <Ignore> prefix (lazy.nvim pattern)"
        );
    }

    #[test]
    fn test_on_map_simple_form_defaults_to_normal_mode() {
        let mut s = make_lazy_plugin("p");
        s.on_map = Some(vec![MapSpec {
            lhs: "<leader>f".to_string(),
            mode: Vec::new(),
            desc: None,
        }]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        // mode 空 → {"n"} にフォールバック
        assert!(
            lua.contains("vim.keymap.set({ \"n\" }"),
            "empty mode should default to normal mode"
        );
    }

    #[test]
    fn test_on_map_table_form_respects_multiple_modes() {
        let mut s = make_lazy_plugin("p");
        s.on_map = Some(vec![MapSpec {
            lhs: "<leader>v".to_string(),
            mode: vec!["n".to_string(), "x".to_string()],
            desc: None,
        }]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        assert!(
            lua.contains("vim.keymap.set({ \"n\", \"x\" }"),
            "multiple modes should be emitted as a Lua list"
        );
    }

    #[test]
    fn test_on_map_table_form_emits_desc_opts() {
        let mut s = make_lazy_plugin("p");
        s.on_map = Some(vec![MapSpec {
            lhs: "<leader>g".to_string(),
            mode: vec!["n".to_string()],
            desc: Some("Grep files".to_string()),
        }]);
        let lua = gen_loader(Path::new("/merged"), &[s]);
        assert!(
            lua.contains("desc = \"Grep files\""),
            "desc should be emitted in keymap opts"
        );
    }

    // ========================================================
    // Sample dump (目視用 ignored test)
    // ========================================================

    #[test]
    #[ignore]
    fn dump_full_sample_loader() {
        // `cargo test dump_full_sample_loader -- --ignored --nocapture` で出力確認用
        let mut plenary = PluginScripts::for_test(
            "plenary",
            "/cache/rvpm/repos/github.com/nvim-lua/plenary.nvim",
        );
        plenary.merge = true;
        plenary.init = Some("/config/init.lua".to_string());
        plenary.plugin_files = vec![
            "/cache/rvpm/repos/github.com/nvim-lua/plenary.nvim/plugin/plenary.vim".to_string(),
        ];

        let mut telescope = PluginScripts::for_test(
            "telescope",
            "/cache/rvpm/repos/github.com/nvim-telescope/telescope.nvim",
        );
        telescope.merge = true;
        telescope.lazy = true;
        telescope.before = Some("/config/tel/before.lua".to_string());
        telescope.after = Some("/config/tel/after.lua".to_string());
        telescope.on_cmd = Some(vec!["Telescope".to_string()]);
        telescope.on_source = Some(vec!["plenary".to_string()]);
        telescope.on_event = Some(vec![
            "BufReadPre".to_string(),
            "User LazyVimStarted".to_string(),
        ]);
        telescope.on_map = Some(vec![
            MapSpec {
                lhs: "<leader>ff".to_string(),
                mode: vec!["n".to_string()],
                desc: Some("Find files".to_string()),
            },
            MapSpec {
                lhs: "<leader>fg".to_string(),
                mode: vec!["n".to_string(), "x".to_string()],
                desc: None,
            },
            MapSpec {
                lhs: "<leader>fb".to_string(),
                mode: Vec::new(),
                desc: None,
            },
        ]);
        telescope.plugin_files = vec![
            "/cache/rvpm/repos/github.com/nvim-telescope/telescope.nvim/plugin/telescope.lua"
                .to_string(),
        ];
        telescope.ftdetect_files = vec![];
        telescope.after_plugin_files = vec![];

        let mut treesitter = PluginScripts::for_test(
            "nvim-treesitter",
            "/cache/rvpm/repos/github.com/nvim-treesitter/nvim-treesitter",
        );
        treesitter.merge = false; // non-merge eager
        treesitter.before = Some("/config/ts/before.lua".to_string());
        treesitter.after = Some("/config/ts/after.lua".to_string());
        treesitter.plugin_files = vec![
            "/cache/rvpm/repos/github.com/nvim-treesitter/nvim-treesitter/plugin/nvim-treesitter.lua".to_string(),
        ];
        treesitter.ftdetect_files = vec![
            "/cache/rvpm/repos/github.com/nvim-treesitter/nvim-treesitter/ftdetect/blade.vim"
                .to_string(),
        ];
        treesitter.after_plugin_files = vec![
            "/cache/rvpm/repos/github.com/nvim-treesitter/nvim-treesitter/after/plugin/query_predicates.lua".to_string(),
        ];

        let scripts = vec![plenary, telescope, treesitter];
        let lua = gen_loader(Path::new("/cache/rvpm/merged"), &scripts);
        println!(
            "\n======== GENERATED LOADER ========\n{}\n==================================\n",
            lua
        );
    }

    // ========================================================
    // 既存テスト (互換性確認)
    // ========================================================

    #[test]
    fn test_load_lazy_fires_user_event() {
        let merged_dir = Path::new("/merged");
        let mut s = PluginScripts::for_test("plenary", "/path/plenary");
        s.lazy = true;
        s.on_cmd = Some(vec!["Plenary".to_string()]);
        let lua = gen_loader(merged_dir, &[s]);
        assert!(
            lua.contains(
                "vim.api.nvim_exec_autocmds(\"User\", { pattern = \"rvpm_loaded_\" .. name })"
            ),
            "load_lazy must fire User autocmd after loading"
        );
    }

    #[test]
    fn test_generate_loader_with_cond() {
        let merged_dir = Path::new("/path/to/merged");
        let mut s = PluginScripts::for_test("cond_lazy", "/path/to/plugin");
        s.lazy = true;
        s.on_cmd = Some(vec!["Cmd".to_string()]);
        s.on_ft = Some(vec!["rust".to_string()]);
        s.on_map = Some(vec![MapSpec {
            lhs: "<leader>f".to_string(),
            mode: Vec::new(),
            desc: None,
        }]);
        s.on_event = Some(vec!["BufRead".to_string()]);
        s.on_path = Some(vec!["*.rs".to_string(), "Cargo.toml".to_string()]);
        s.on_source = Some(vec!["plenary.nvim".to_string()]);
        s.cond = Some("vim.fn.has('win32') == 1".to_string());
        let lua = gen_loader(merged_dir, &[s]);

        assert!(lua.contains("if vim.fn.has('win32') == 1 then"));
        assert!(lua.contains("nvim_create_user_command(\"Cmd\""));
        assert!(lua.contains("pattern = { \"rust\" }"));
        assert!(lua.contains("vim.keymap.set({ \"n\" }, \"<leader>f\""));
        assert!(lua.contains("nvim_create_autocmd({ \"BufRead\" }"));
        assert!(lua.contains("pattern = { \"*.rs\", \"Cargo.toml\" }"));
        assert!(lua.contains("pattern = { \"rvpm_loaded_plenary.nvim\" }"));
    }

    // ========================================================
    // promote_lazy_to_eager 単体テスト
    // ========================================================

    #[test]
    fn test_promote_lazy_to_eager_returns_promoted_names() {
        let mut a = PluginScripts::for_test("plenary.nvim", "/path/plenary");
        a.lazy = true;
        a.merge = true;

        let mut b = PluginScripts::for_test("telescope.nvim", "/path/telescope");
        b.lazy = false;
        b.depends = Some(vec!["plenary.nvim".to_string()]);

        let mut scripts = vec![a, b];
        let promoted = promote_lazy_to_eager(&mut scripts);

        assert_eq!(promoted, vec!["plenary.nvim".to_string()]);
        assert!(!scripts[0].lazy, "plenary should be promoted to eager");
        assert!(!scripts[1].lazy, "telescope should remain eager");
    }

    #[test]
    fn test_promote_lazy_to_eager_chain() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.lazy = true;

        let mut b = PluginScripts::for_test("b", "/path/b");
        b.lazy = true;
        b.depends = Some(vec!["a".to_string()]);

        let mut c = PluginScripts::for_test("c", "/path/c");
        c.lazy = false;
        c.depends = Some(vec!["b".to_string()]);

        let mut scripts = vec![a, b, c];
        let promoted = promote_lazy_to_eager(&mut scripts);

        assert!(promoted.contains(&"a".to_string()));
        assert!(promoted.contains(&"b".to_string()));
        assert!(!scripts[0].lazy);
        assert!(!scripts[1].lazy);
    }

    #[test]
    fn test_promote_lazy_to_eager_no_promotion_needed() {
        let a = PluginScripts::for_test("a", "/path/a");
        let mut b = PluginScripts::for_test("b", "/path/b");
        b.lazy = true;
        b.on_cmd = Some(vec!["Cmd".to_string()]);

        let mut scripts = vec![a, b];
        let promoted = promote_lazy_to_eager(&mut scripts);

        assert!(promoted.is_empty());
        assert!(!scripts[0].lazy);
        assert!(scripts[1].lazy);
    }
}
