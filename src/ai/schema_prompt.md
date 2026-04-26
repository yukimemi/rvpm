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

The three hook files differ along **two axes**: *when in the boot sequence they run*, and *whether they run for lazy plugins at Neovim startup or at trigger time*.

- `init.lua` — runs at **Neovim startup**, **before RTP append**, for **every** plugin including lazy ones. This is the only hook that fires at startup for a lazy plugin (the other two wait until the trigger fires). **Rare.** Use it only when:
  - You want some side effect to happen at Neovim startup even though the plugin itself is lazy (e.g. register a `VimEnter` autocmd, or set a flag another startup-time plugin reads), OR
  - Something must run before any plugin code is on `runtimepath` (very rare).
  - **Most plugins don't need init.lua.** If you'd just be setting `vim.g.<plugin>_xxx` for the plugin's own use, that belongs in `before.lua`, not here.
- `before.lua` — runs at **plugin load time** (immediately for eager, at trigger time for lazy), **after** RTP append, **before** the plugin's `plugin/*` is sourced. **This is the standard place for pre-source config:** `vim.g.<plugin>_xxx = ...` style options that the plugin's `plugin/*` reads when it sources, or any other pre-load tweak the README documents.
- `after.lua` — runs at **plugin load time**, **after** the plugin's `plugin/*` is sourced. **This is the default place for `require('foo').setup({...})`** and keymaps. Modern Lua plugins almost always document their setup as "call `require('foo').setup({...})`" with no ordering constraint — that means after.lua.

**Default rule for `setup({...})`: put it in `after.lua`.** Move it to `before.lua` only when the README has an explicit "this must run before the plugin's `plugin/*`" / "call this before the plugin is sourced" instruction. rvpm's `after.lua` is the equivalent of lazy.nvim's `config = function() ... end` and packer's `config = ...`.

**Default rule for `vim.g.<plugin>_xxx = ...`: put it in `before.lua`.** Only move it to `init.lua` if you specifically need that variable set at Neovim startup time (rare — most `vim.g` plugin options only need to be set before the plugin's `plugin/*` sources, which is exactly what before.lua provides).

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

### Merged variants (REQUIRED when existing content is provided)

When the "User context" section below contains an **existing `[[plugins]]` entry** or **existing hook file body**, you MUST emit an additional `_merged` tag for **each** section that has existing content. The user will be shown both versions side-by-side and pick one.

- `<rvpm:plugin_entry>` — your **clean redesign**, treating the entry as if you were configuring this plugin from scratch (you may drop or rename fields you think are wrong).
- `<rvpm:plugin_entry_merged>` — your **conservative merge**: preserve the user's intent (custom names, `rev` pins, custom triggers they've added, ordering) and only adjust where you're confident the change is an improvement.
- `<rvpm:after_lua>` / `<rvpm:after_lua_merged>` — same split for `after.lua`. Fresh = ignore existing body. Merged = preserve user-added keymaps / custom blocks, integrate your additions.
- Same convention for `<rvpm:init_lua_merged>` and `<rvpm:before_lua_merged>`.

Rules for the merged variant:

1. **Don't drop user-added content silently.** Keymaps, autocmds, custom helper functions, comments — preserve them unless they're clearly broken.
2. **Don't duplicate.** If you'd add the same line that's already there, just keep it once.
3. **Order matters for Lua hooks.** `vim.g.<plugin>_xxx = ...` must run before `require('plugin').setup({})` — keep the user's ordering or fix it if broken.
4. **For `[[plugins]]` entry merged**: keep `name`, `url`, `rev`, `dev`, `dst`, `cond`, `build`, `build_lua`, `depends` intact unless you have a strong reason to change them. `on_*` triggers are where you'd typically refine.
5. **If there's nothing meaningful to merge** (existing is empty or unrelated), output `(none)` in the `_merged` tag and keep the fresh variant alone.

When **no existing content is provided** for a section, omit the `_merged` tag entirely (or output `(none)` — both are accepted).

## Constraints

- Match the user's `url_style` (`short` = `owner/repo`, `full` = `https://github.com/owner/repo`) — see User context.
- For `depends`, only list plugins present in the user's `config.toml`.
- Don't repeat the user's existing `[[plugins]]` entries — output only the **new** entry for this plugin.
- Don't write hook files unless the plugin's README clearly demonstrates the configuration.
- TOML must be syntactically valid. Strings need quotes. Arrays use brackets.
- When uncertain, prefer eager (no triggers) over guessing wrong triggers — wrong triggers cause silent load failures.
