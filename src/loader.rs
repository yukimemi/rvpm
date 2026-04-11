use std::path::Path;
use crate::config::MapSpec;

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

pub fn generate_loader(merged_dir: &Path, scripts: &[PluginScripts]) -> String {
    let mut lua = String::new();
    lua.push_str("-- rvpm generated loader.lua\n\n");

    // ======================================================
    // Phase 0: Neovim の auto-source を無効化 (lazy.nvim 方式)
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
    // Phase 1: 全プラグインの init.lua (依存順)
    // init は "pre-rtp" phase であり、全プラグイン共通
    // ======================================================
    for s in scripts {
        if let Some(init) = &s.init {
            let body = format!("dofile(\"{}\")\n", init.replace('\\', "/"));
            push_with_cond(&mut lua, &s.cond, &body);
        }
    }
    lua.push('\n');

    // ======================================================
    // Phase 2: merge=true プラグインがあれば merged rtp を 1 回 append
    // ======================================================
    if scripts.iter().any(|s| s.merge) {
        let merged_path = merged_dir.to_string_lossy().replace('\\', "/");
        lua.push_str(&format!("vim.opt.rtp:append(\"{}\")\n\n", merged_path));
    }

    // ======================================================
    // Phase 3: eager プラグイン処理 (依存順)
    // 非 merge: rtp 追加 → before → plugin/ → ftdetect/ → after/plugin/ → after
    // merge   : before → plugin/ → ftdetect/ → after/plugin/ → after
    // 事前 glob 済みのファイルを直接 source する (起動時 glob 不要)
    // ======================================================
    for s in scripts {
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
    // Phase 4: lazy プラグインの trigger 登録
    // 各プラグインの plugin/ ftdetect/ after/plugin ファイルリストを
    // ローカル変数として emit し、trigger closure から参照する
    // ======================================================
    for s in scripts {
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
        // ファイルリストをローカルテーブルとして宣言
        body.push_str(&format!("local {} = {}\n", pf_var, lua_str_list(&s.plugin_files)));
        body.push_str(&format!("local {} = {}\n", fd_var, lua_str_list(&s.ftdetect_files)));
        body.push_str(&format!("local {} = {}\n", ap_var, lua_str_list(&s.after_plugin_files)));

        let load_call = format!(
            "load_lazy(\"{}\", \"{}\", {}, {}, {}, {}, {})",
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
            let patterns: Vec<String> = sources.iter().map(|src| format!("rvpm_loaded_{}", src)).collect();
            body.push_str(&format!(
                "vim.api.nvim_create_autocmd(\"User\", {{ pattern = {{ \"{}\" }}, once = true, callback = function()\n\
                 \x20 {load}\n\
                 end }})\n",
                patterns.join("\", \""),
                load = load_call,
            ));
        }

        push_with_cond(&mut lua, &s.cond, &body);
    }

    lua
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================
    // 新モデル: lazy.nvim 方式 + merge optimization + 事前コンパイル
    // ========================================================

    #[test]
    fn test_loader_disables_neovim_plugin_loading() {
        let lua = generate_loader(Path::new("/merged"), &[]);
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
        let lua = generate_loader(Path::new("/merged"), &[s]);
        let init_pos = lua.find("/cfg/a/init.lua").expect("init missing");
        let rtp_pos = lua.find("vim.opt.rtp:append(\"/merged\")").expect("merged rtp missing");
        let before_pos = lua.find("/cfg/a/before.lua").expect("before missing");
        assert!(init_pos < rtp_pos, "init must come BEFORE merged rtp append");
        assert!(rtp_pos < before_pos, "before must come AFTER rtp append");
    }

    #[test]
    fn test_loader_merged_rtp_appended_exactly_once() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.merge = true;
        let mut b = PluginScripts::for_test("b", "/path/b");
        b.merge = true;
        let lua = generate_loader(Path::new("/merged"), &[a, b]);
        let count = lua.matches("vim.opt.rtp:append(\"/merged\")").count();
        assert_eq!(count, 1, "merged rtp should be appended exactly once for multiple merge=true plugins");
    }

    #[test]
    fn test_loader_no_merged_rtp_when_all_non_merge() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.merge = false;
        let lua = generate_loader(Path::new("/merged"), &[a]);
        assert!(
            !lua.contains("vim.opt.rtp:append(\"/merged\")"),
            "should NOT append merged rtp when no merge=true plugin exists"
        );
    }

    #[test]
    fn test_loader_non_merge_eager_appends_own_rtp() {
        let mut a = PluginScripts::for_test("solo", "/path/solo");
        a.merge = false;
        let lua = generate_loader(Path::new("/merged"), &[a]);
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
        let lua = generate_loader(Path::new("/merged"), &[a]);
        let before_pos = lua.find("/cfg/a/before.lua").unwrap();
        let source_pos = lua.find("vim.cmd(\"source /path/a/plugin/a.vim\")").expect("plugin file source missing");
        let after_pos = lua.find("/cfg/a/after.lua").unwrap();
        assert!(before_pos < source_pos, "before.lua must come before plugin/ source");
        assert!(source_pos < after_pos, "after.lua must come after plugin/ source");
    }

    #[test]
    fn test_loader_eager_wraps_ftdetect_in_filetypedetect_augroup() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.merge = true;
        a.ftdetect_files = vec!["/path/a/ftdetect/a.vim".to_string()];
        let lua = generate_loader(Path::new("/merged"), &[a]);
        // eager phase 内の ftdetect source を探し、その直前/直後に augroup begin/end があるか確認
        // (load_lazy helper 内の augroup とは別に、Phase 3 の augroup が必要)
        let ftdetect_source_pos = lua
            .find("vim.cmd(\"source /path/a/ftdetect/a.vim\")")
            .expect("ftdetect source missing");
        // source の手前に "augroup filetypedetect" (source より前の範囲で rfind)
        let prior = &lua[..ftdetect_source_pos];
        let augroup_begin_pos = prior.rfind("augroup filetypedetect").expect("augroup begin missing before ftdetect source");
        // source の後ろに "augroup END"
        let after = &lua[ftdetect_source_pos..];
        let augroup_end_rel = after.find("augroup END").expect("augroup END missing after ftdetect source");
        // source と begin/end の間に他の augroup END/begin がないことも軽く確認
        assert!(augroup_begin_pos < ftdetect_source_pos);
        assert!(augroup_end_rel > 0);
    }

    #[test]
    fn test_loader_eager_sources_after_plugin_files() {
        let mut a = PluginScripts::for_test("a", "/path/a");
        a.merge = true;
        a.after_plugin_files = vec!["/path/a/after/plugin/a.vim".to_string()];
        let lua = generate_loader(Path::new("/merged"), &[a]);
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
        let lua = generate_loader(Path::new("/merged"), &[a]);
        // lazy plugin の plugin_files は eager の位置で直接 source されない
        // (load_lazy 経由で動的に呼ばれる)
        // eager の source とは区別して、 trigger 経由でのみ呼ばれる
        // → top-level に "vim.cmd(\"source /path/a/plugin/a.vim\")" が直接出てはいけない
        // (load_lazy の中のローカルテーブル内には出ていい)
        let direct_source_count = lua
            .lines()
            .filter(|l| l.trim_start().starts_with("vim.cmd(\"source /path/a/plugin/a.vim\")"))
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
        let lua = generate_loader(Path::new("/merged"), &[a]);
        // ファイルリストがどこかに登場すること (ローカルテーブルとしてでも load_lazy 引数内でも OK)
        assert!(lua.contains("/path/a/plugin/a.vim"), "plugin file must be referenced");
        assert!(lua.contains("/path/a/ftdetect/a.vim"), "ftdetect file must be referenced");
        assert!(lua.contains("/path/a/after/plugin/a.vim"), "after/plugin file must be referenced");
    }

    #[test]
    fn test_load_lazy_helper_sources_ftdetect_in_augroup() {
        let lua = generate_loader(Path::new("/merged"), &[]);
        // load_lazy 関数定義の中に ftdetect の処理が augroup 付きで入っているか
        let load_lazy_start = lua.find("local function load_lazy").expect("load_lazy definition missing");
        let end_marker = lua[load_lazy_start..].find("\nend\n").expect("load_lazy end missing") + load_lazy_start;
        let body = &lua[load_lazy_start..end_marker];
        assert!(body.contains("augroup filetypedetect"), "load_lazy must wrap ftdetect in filetypedetect augroup");
        assert!(body.contains("augroup END"), "load_lazy must close the augroup");
    }

    // ========================================================
    // Lazy trigger 改善テスト (lazy.nvim 参考)
    // ========================================================

    fn make_lazy_plugin(name: &str) -> PluginScripts {
        let mut s = PluginScripts::for_test(name, &format!("/path/{}", name));
        s.lazy = true;
        s
    }

    #[test]
    fn test_on_cmd_handler_has_bang_range_complete_options() {
        let mut s = make_lazy_plugin("tel");
        s.on_cmd = Some(vec!["Telescope".to_string()]);
        let lua = generate_loader(Path::new("/merged"), &[s]);
        // user command 定義に bang/range/complete オプションが入っている
        assert!(lua.contains("bang = true"), "on_cmd must enable bang");
        assert!(lua.contains("range = true"), "on_cmd must enable range");
        assert!(lua.contains("complete ="), "on_cmd must provide complete callback");
        assert!(lua.contains("nargs = \"*\""), "on_cmd still supports any args");
    }

    #[test]
    fn test_on_cmd_handler_reconstructs_command_from_event() {
        let mut s = make_lazy_plugin("tel");
        s.on_cmd = Some(vec!["Telescope".to_string()]);
        let lua = generate_loader(Path::new("/merged"), &[s]);
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
        let lua = generate_loader(Path::new("/merged"), &[s]);
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
        let lua = generate_loader(Path::new("/merged"), &[s]);
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
        let lua = generate_loader(Path::new("/merged"), &[s]);
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
        let lua = generate_loader(Path::new("/merged"), &[s]);
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
        let lua = generate_loader(Path::new("/merged"), &[s]);
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
        let lua = generate_loader(Path::new("/merged"), &[s]);
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
        let lua = generate_loader(Path::new("/merged"), &[s]);
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
        let lua = generate_loader(Path::new("/merged"), &[s]);
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
        let lua = generate_loader(Path::new("/merged"), &[s]);
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
        let lua = generate_loader(Path::new("/merged"), &[s]);
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
        let mut plenary = PluginScripts::for_test("plenary", "/cache/rvpm/repos/github.com/nvim-lua/plenary.nvim");
        plenary.merge = true;
        plenary.init = Some("/config/init.lua".to_string());
        plenary.plugin_files = vec![
            "/cache/rvpm/repos/github.com/nvim-lua/plenary.nvim/plugin/plenary.vim".to_string(),
        ];

        let mut telescope = PluginScripts::for_test("telescope", "/cache/rvpm/repos/github.com/nvim-telescope/telescope.nvim");
        telescope.merge = true;
        telescope.lazy = true;
        telescope.before = Some("/config/tel/before.lua".to_string());
        telescope.after = Some("/config/tel/after.lua".to_string());
        telescope.on_cmd = Some(vec!["Telescope".to_string()]);
        telescope.on_source = Some(vec!["plenary".to_string()]);
        telescope.on_event = Some(vec!["BufReadPre".to_string(), "User LazyVimStarted".to_string()]);
        telescope.on_map = Some(vec![
            MapSpec { lhs: "<leader>ff".to_string(), mode: vec!["n".to_string()], desc: Some("Find files".to_string()) },
            MapSpec { lhs: "<leader>fg".to_string(), mode: vec!["n".to_string(), "x".to_string()], desc: None },
            MapSpec { lhs: "<leader>fb".to_string(), mode: Vec::new(), desc: None },
        ]);
        telescope.plugin_files = vec![
            "/cache/rvpm/repos/github.com/nvim-telescope/telescope.nvim/plugin/telescope.lua".to_string(),
        ];
        telescope.ftdetect_files = vec![];
        telescope.after_plugin_files = vec![];

        let mut treesitter = PluginScripts::for_test("nvim-treesitter", "/cache/rvpm/repos/github.com/nvim-treesitter/nvim-treesitter");
        treesitter.merge = false; // non-merge eager
        treesitter.before = Some("/config/ts/before.lua".to_string());
        treesitter.after = Some("/config/ts/after.lua".to_string());
        treesitter.plugin_files = vec![
            "/cache/rvpm/repos/github.com/nvim-treesitter/nvim-treesitter/plugin/nvim-treesitter.lua".to_string(),
        ];
        treesitter.ftdetect_files = vec![
            "/cache/rvpm/repos/github.com/nvim-treesitter/nvim-treesitter/ftdetect/blade.vim".to_string(),
        ];
        treesitter.after_plugin_files = vec![
            "/cache/rvpm/repos/github.com/nvim-treesitter/nvim-treesitter/after/plugin/query_predicates.lua".to_string(),
        ];

        let scripts = vec![plenary, telescope, treesitter];
        let lua = generate_loader(Path::new("/cache/rvpm/merged"), &scripts);
        println!("\n======== GENERATED LOADER ========\n{}\n==================================\n", lua);
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
        let lua = generate_loader(merged_dir, &[s]);
        assert!(
            lua.contains("vim.api.nvim_exec_autocmds(\"User\", { pattern = \"rvpm_loaded_\" .. name })"),
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
        let lua = generate_loader(merged_dir, &[s]);

        assert!(lua.contains("if vim.fn.has('win32') == 1 then"));
        assert!(lua.contains("nvim_create_user_command(\"Cmd\""));
        assert!(lua.contains("pattern = { \"rust\" }"));
        assert!(lua.contains("vim.keymap.set({ \"n\" }, \"<leader>f\""));
        assert!(lua.contains("nvim_create_autocmd({ \"BufRead\" }"));
        assert!(lua.contains("pattern = { \"*.rs\", \"Cargo.toml\" }"));
        assert!(lua.contains("pattern = { \"rvpm_loaded_plenary.nvim\" }"));
    }
}

