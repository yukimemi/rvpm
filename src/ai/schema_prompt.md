# rvpm — TOML schema brief (for AI assistants)

You are configuring **rvpm**, a Rust-based Neovim plugin manager. Generate
the optimal `[[plugins]]` TOML block for the plugin the user is adding,
plus optional `init.lua` / `before.lua` / `after.lua` hook files.

## Plugin entry schema

```toml
[[plugins]]
name = "..."          # display name. defaults to repo name if omitted.
url  = "owner/repo"   # GitHub short form OR https://github.com/owner/repo (match the user's existing url_style — see "User context" below)
rev  = "v1.2.3"       # optional pin: branch / tag / commit SHA
merge   = true        # optional, default true. set false to keep plugin's runtimepath isolated when it conflicts with others
depends = ["other-plugin"]  # optional. ONLY list plugins already present in the user's config.

# Lazy triggers (any one fires loading). Setting any of these auto-promotes the plugin to lazy.
on_cmd   = ["Foo", "/^Bar/"]                # exact :command names OR /regex/ (regex expanded against the plugin's statically-defined command names)
on_ft    = ["rust", "toml"]                 # filetype names
on_event = ["BufReadPre", "User LazyDone", "/^User Foo/"]  # standard or User events. /regex/ matches "User <name>" synthesized strings
on_path  = ["*.rs", "Cargo.toml"]           # BufRead/BufNewFile glob
on_source = ["plenary.nvim"]                # load when another plugin's "rvpm_loaded_<name>" User autocmd fires
# on_map: simple LHS string OR { lhs, mode, desc } table. Mix freely.
# /regex/ for lhs expands against the plugin's <Plug>(...) mappings. mode/desc are inherited per expanded entry.
on_map = [
  "<leader>f",
  { lhs = "<leader>v", mode = ["n", "x"] },
  { lhs = "/^<Plug>\\(Chezmoi/", mode = ["n"] },
]

cond = "vim.fn.has('win32') == 1"   # optional Lua expression; load only when truthy
```

## Hook file conventions

- `init.lua` — runs **before** RTP append. Use for `vim.g.<plugin>_setting = ...` global config.
- `before.lua` — runs **after** RTP append, before `plugin/*` source. Use for `require('foo').setup(...)`.
- `after.lua` — runs **after** `plugin/*` source. Use for keymaps and post-setup tweaks.

If the plugin needs no special hook, output `(none)` for that section. Don't invent hooks "just in case."

## Lazy trigger guidance

- If the plugin exposes commands that match a clear prefix (`FooOpen`, `FooClose`, `FooToggle`) prefer `on_cmd = ["/^Foo/"]` over enumerating each.
- If it's a `<Plug>`-based plugin (vim-commentary, vim-surround, vim-unimpaired) prefer `on_map = [{ lhs = "/^<Plug>\\(Foo/", mode = [...] }]`.
- If it fires User events as a "ready" signal (`PluginLoaded` etc.), other plugins can hook via `on_event = ["User PluginLoaded"]`.
- If it's a colorscheme plugin (`colors/foo.{vim,lua}`), it'll be auto-handled by rvpm's ColorSchemePre — no trigger needed (just leave eager or use any trigger).
- If you cannot identify a lazy trigger from the README, leave triggers unset (eager). Eager is correct for plugins that need to register autocmds on `VimEnter`.

## Output format (REQUIRED)

Reply with exactly these XML tags. No markdown code fences, no preamble, no explanation outside the tags:

```
<rvpm:plugin_entry>
[[plugins]]
url = "..."
... TOML body ...
</rvpm:plugin_entry>

<rvpm:init_lua>
... Lua source, or (none) if no init.lua needed ...
</rvpm:init_lua>

<rvpm:before_lua>
... Lua source, or (none) ...
</rvpm:before_lua>

<rvpm:after_lua>
... Lua source, or (none) ...
</rvpm:after_lua>

<rvpm:explanation>
2-3 sentences explaining the choices. Reference specific README snippets, depends already in user config, why these triggers (or eager).
</rvpm:explanation>
```

## Constraints

- Match the user's `url_style` (`short` = `owner/repo`, `full` = `https://github.com/owner/repo`) — see User context.
- For `depends`, only list plugins present in the user's `config.toml`.
- Don't repeat the user's existing `[[plugins]]` entries — output only the **new** entry for this plugin.
- Don't write hook files unless the plugin's README clearly demonstrates the configuration.
- TOML must be syntactically valid. Strings need quotes. Arrays use brackets.
- When uncertain, prefer eager (no triggers) over guessing wrong triggers — wrong triggers cause silent load failures.
