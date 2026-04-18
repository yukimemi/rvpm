# rvpm

> **R**ust-based **V**im **P**lugin **M**anager — a fast, pre-compiled plugin manager for Neovim

[![CI](https://github.com/yukimemi/rvpm/actions/workflows/ci.yml/badge.svg)](https://github.com/yukimemi/rvpm/actions/workflows/ci.yml)
[![Release](https://github.com/yukimemi/rvpm/actions/workflows/release.yml/badge.svg)](https://github.com/yukimemi/rvpm/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

![demo](demo.gif)

rvpm clones plugins in parallel, links `merge = true` plugins into a single
runtime-path entry, and ahead-of-time compiles a static `loader.lua` that
sources everything without any runtime glob cost.

## Why rvpm?

- **CLI-first** — manage plugins from your terminal, not from inside Neovim
- **TOML config** — declarative plugin specs with Tera template support
- **Pre-compiled loader** — `rvpm generate` walks plugin directories at CLI
  time and bakes the file list into `loader.lua`; Neovim just sources a
  fixed list of `dofile()` / `source` calls
- **Full lazy-loading** — `on_cmd`, `on_ft`, `on_map`, `on_event`, `on_path`,
  `on_source`, auto-detected `ColorSchemePre`, and `depends`-aware loading
- **Merge optimization** — `merge = true` plugins share a single rtp entry
- **Plugin discovery TUI** — `rvpm store` browses the GitHub `neovim-plugin`
  topic with live README preview; `Tab` switches focus between panes
- **Resilient** — cyclic dependencies, missing plugins, and config errors
  produce warnings, not crashes

<details>
<summary><b>More features</b></summary>

- **Fast startup** — 9-phase loader model with `vim.go.loadplugins = false`
  and pre-globbed `plugin/` / `ftdetect/` / `after/plugin/` file lists
- **Global hooks** — `before.lua` / `after.lua` alongside `config.toml`
  are auto-detected at generate time; no config entry needed
- **Lazy trigger fidelity** — `User Xxx` pattern shorthand, bang/range/count/
  complete-aware commands, keymaps with mode + desc, and `<Ignore>`-prefixed
  replay for safety; operator-pending mode preserves `v:operator` /
  `v:count1` / `v:register`
- **Colorscheme auto-detection** — lazy plugins whose clone contains a
  `colors/*.vim` or `colors/*.lua` file automatically gain a `ColorSchemePre`
  autocmd handler so `:colorscheme <name>` loads the plugin on demand
- **Dependency ordering** — topological sort on `depends`, resilient to
  cycles and missing references; eager→lazy deps are auto-promoted
- **Windows first-class** — hardcoded `~/.config` / `~/.cache` layout for
  dotfiles portability, junction instead of symlink to avoid permission
  issues
- **Interactive TUI** — `rvpm list` with sync / update / generate / remove /
  edit / set action keys
- **CLI-driven set** — `rvpm set foo --on-event '["BufReadPre","User Started"]'`
  or full JSON object form for `on_map` with mode/desc
- **TOML direct edit escape hatch** — `rvpm config` / `rvpm set` sub-menu to
  jump to the plugin's block in `$EDITOR`
- **Init.lua integration** — `rvpm init --write` wires the generated loader
  into `~/.config/$NVIM_APPNAME/init.lua` (creates the file if missing)

</details>

## Installation

```sh
# From crates.io
cargo install rvpm

# Or from source (latest main)
cargo install --git https://github.com/yukimemi/rvpm
```

Pre-built binaries are also available on the
[Releases](https://github.com/yukimemi/rvpm/releases) page for Linux
(x86_64), macOS (Intel / Apple Silicon), and Windows (x86_64). Extract the
binary into any directory on your `PATH`.

## Quick start

```sh
# 1. One-time setup (creates both config.toml and init.lua)
rvpm init --write
# → ~/.config/rvpm/<appname>/config.toml  (plugin configuration)
# → ~/.config/nvim/init.lua               (loader wiring)
# <appname> = $RVPM_APPNAME → $NVIM_APPNAME → "nvim"

# 2. Add plugins
rvpm add folke/snacks.nvim
rvpm add nvim-telescope/telescope.nvim

# 3. Browse the GitHub "neovim-plugin" topic and install from the TUI
rvpm store

# 4. Manage installed plugins interactively
rvpm list

# 5. Open config.toml to tweak settings (lazy, triggers, etc.)
rvpm config
```

## Configuration

`~/.config/rvpm/<appname>/config.toml`:

```toml
[vars]
# Your own variables, referenced via Tera templates {{ vars.xxx }}
nvim_rc = "~/.config/nvim/rc"

[options]
# Root of all rvpm config (config.toml, global hooks, plugins/ subdir)
# Default: ~/.config/rvpm/<appname>
# config_root = "{{ vars.nvim_rc }}"
# Root of all rvpm cache (clones, merged rtp, loader.lua, store cache)
# Default: ~/.cache/rvpm/<appname>
# cache_root = "~/dotfiles/nvim/rvpm"
# Parallel git operations limit (default: 8)
concurrency = 10
# Auto-sync config.toml / global hooks / per-plugin hooks back to chezmoi
# source state after mutations (default: false). Requires `chezmoi` in PATH.
# chezmoi = true

# [options.store]
# Optional: run READMEs in the store TUI through an external renderer.
# See "rvpm store — plugin discovery TUI > External README renderer".
# readme_command = ["mdcat"]

[[plugins]]
name  = "snacks"
url   = "folke/snacks.nvim"
# No on_* triggers → eager (loaded at startup)

[[plugins]]
name      = "telescope"
url       = "nvim-telescope/telescope.nvim"
depends   = ["snacks.nvim"]
# on_cmd is set → lazy is auto-inferred as true
on_cmd    = ["Telescope"]
on_source = ["snacks.nvim"]

[[plugins]]
url      = "neovim/nvim-lspconfig"
# on_ft / on_event → auto lazy
on_ft    = ["rust", "toml", "lua"]
on_event = ["BufReadPre", "User LazyVimStarted"]

[[plugins]]
name = "which-key"
url  = "folke/which-key.nvim"
# on_map → auto lazy
on_map = [
  "<leader>?",
  { lhs = "<leader>v", mode = ["n", "x"], desc = "Visual leader" },
]
```

### `[options]` reference

rvpm reads `config.toml` from `~/.config/rvpm/<appname>/config.toml`, where
`<appname>` resolves to:

1. `$RVPM_APPNAME` if set, else
2. `$NVIM_APPNAME` if set, else
3. `"nvim"` (default)

This mirrors Neovim's `$NVIM_APPNAME` convention, so running
`NVIM_APPNAME=nvim-test nvim` pairs with `NVIM_APPNAME=nvim-test rvpm sync`
for fully isolated test configs.

| Key | Type | Default | Description |
|---|---|---|---|
| `config_root` | `string` | `~/.config/rvpm/<appname>` | Root for all rvpm config (`config.toml`, global `before.lua` / `after.lua`, and `plugins/<host>/<owner>/<repo>/` per-plugin hooks). Supports `~` and Tera templates. **Recommended: leave unset** |
| `cache_root` | `string` | `~/.cache/rvpm/<appname>` | Root for all rvpm cache (`plugins/repos/`, `plugins/merged/`, `plugins/loader.lua`, `store/`). **Recommended: leave unset** |
| `concurrency` | `integer` | `8` | Max number of parallel git operations during `sync` / `update`. Kept moderate to avoid GitHub rate limits |
| `chezmoi` | `boolean` | `false` | When `true`, rvpm automatically runs `chezmoi re-add` (or `chezmoi add` for new files whose ancestor is already managed) after mutating `config.toml`, global hooks, or per-plugin hooks. Warns if `chezmoi` is not in `PATH`. See [chezmoi integration](#chezmoi-integration) |

> **Symmetric layout.** `config_root` and `cache_root` are structurally
> parallel — each owns a `plugins/` subdirectory at the same depth:
>
> ```text
> ~/.config/rvpm/<appname>/                ← config_root
>     ├── config.toml
>     ├── before.lua       (global, phase 3)
>     ├── after.lua        (global, phase 9)
>     └── plugins/<host>/<owner>/<repo>/   (per-plugin init/before/after.lua)
>
> ~/.cache/rvpm/<appname>/                 ← cache_root
>     ├── plugins/
>     │   ├── repos/<host>/<owner>/<repo>/  (clones)
>     │   ├── merged/                       (linked rtp for merge=true)
>     │   └── loader.lua                    (generated)
>     └── store/                            (`rvpm store` cache)
> ```
>
> **💡 Leave `config_root` and `cache_root` unset** — the defaults are already
> `<appname>`-aware. Setting a literal path (e.g. `cache_root = "~/dotfiles/rvpm"`)
> **breaks appname isolation**: every `$NVIM_APPNAME` variant will share the same
> cache. If you need a custom root *and* appname isolation, use a Tera template:
> ```toml
> cache_root = "~/dotfiles/rvpm/{{ env.NVIM_APPNAME }}"
> ```
> Prefer `~/` over `{{ env.HOME }}` — `~` is portable (Windows too), while
> `$HOME` is not set on Windows.

### chezmoi integration

If you manage your dotfiles with [chezmoi](https://www.chezmoi.io/) and want
rvpm's config changes to land in the chezmoi source state automatically, set:

```toml
[options]
chezmoi = true
```

With this enabled, after every mutation (`rvpm add` / `set` / `remove` /
`edit` / `config`, plus TUI actions `e` / `s` / `d` that edit hooks,
plugin options, or remove plugins), rvpm:

- runs `chezmoi re-add <path>` when the target file is already chezmoi-managed, and
- runs `chezmoi add <path>` when the file is newly created by rvpm and its
  nearest existing ancestor directory is chezmoi-managed (typical for newly
  created per-plugin hook files under `plugins/<host>/<owner>/<repo>/`).

Files whose ancestors are not managed by chezmoi are left alone, so this is
safe to enable even if only part of your rvpm tree lives in chezmoi.

If `options.chezmoi = true` but the `chezmoi` binary isn't in `PATH`, rvpm
prints a warning on every mutation (since you explicitly opted in — the
loud feedback nudges you to either install chezmoi or set
`chezmoi = false`). The primary rvpm operation still succeeds.

Detection uses `chezmoi source-path <path>` (exit 0 = managed).

### Tera templates

The entire `config.toml` is processed by [Tera](https://keats.github.io/tera/)
before TOML parsing. You can use `{{ vars.xxx }}`, `{{ env.HOME }}`,
`{{ is_windows }}`, `{% if %}` blocks, and more.

<details>
<summary><b>Available context & examples</b></summary>

#### Context

| Variable | Type | Description |
|---|---|---|
| `vars.*` | any | User-defined variables from `[vars]` |
| `env.*` | string | Environment variables (e.g. `{{ env.HOME }}`) |
| `is_windows` | bool | `true` on Windows, `false` otherwise |

#### Variables referencing other variables

Variables can reference each other — including forward references:

```toml
[vars]
base = "~/.cache/rvpm"
full = "{{ vars.base }}/custom"   # → "~/.cache/rvpm/custom"

# Forward reference works too
greeting = "Hello {{ vars.name }}"
name = "yukimemi"
# greeting → "Hello yukimemi"
```

#### Conditional plugin inclusion

Use `{% if %}` to completely exclude plugins from `loader.lua` at generate time:

```toml
[vars]
use_blink = true
use_cmp = false
use_snacks = true

[options]

# ── Completion: pick one ─────────────────────────
{% if vars.use_blink %}
[[plugins]]
url = "saghen/blink.cmp"
on_event = ["InsertEnter", "CmdlineEnter"]
{% endif %}

{% if vars.use_cmp %}
[[plugins]]
url = "hrsh7th/nvim-cmp"
on_event = "InsertEnter"
{% endif %}

{% if vars.use_snacks %}
[[plugins]]
url = "folke/snacks.nvim"
{% endif %}
```

#### Platform-specific plugins

```toml
{% if is_windows %}
[[plugins]]
url = "thinca/vim-winenv"
{% endif %}

[[plugins]]
url = "folke/snacks.nvim"
cond = "{{ is_windows }}"  # runtime cond: included in loader but guarded
```

> **`{% if %}` vs `cond`**: `{% if %}` removes the plugin entirely at generate
> time — it won't be cloned, merged, or appear in `loader.lua`. `cond` keeps
> the plugin in `loader.lua` but wraps it in `if <expr> then ... end` for
> runtime evaluation.

</details>

### `[[plugins]]` reference

<details>
<summary><b>All plugin fields</b></summary>

| Key | Type | Default | Description |
|---|---|---|---|
| `url` | `string` | **(required)** | Plugin repository. `owner/repo` (GitHub shorthand), full URL, or local path |
| `name` | `string` | repo name from `url` (e.g. `telescope.nvim`) | Friendly name used in `rvpm_loaded_<name>` User autocmd, `on_source` chain, and log messages. Auto-derived by taking the last path component of the URL and stripping `.git` |
| `dst` | `string` | `{cache_root}/plugins/repos/<host>/<owner>/<repo>` | Custom clone destination (overrides the default path layout) |
| `lazy` | `bool` | auto | **Auto-inferred**: if any `on_*` trigger is set, defaults to `true`; otherwise `false`. Write `lazy = false` explicitly to force eager loading even with triggers |
| `merge` | `bool` | `true` | If `true`, the plugin directory is linked into `{cache_root}/plugins/merged/` and shares a single runtimepath entry |
| `rev` | `string` | HEAD | Branch, tag, or commit hash to check out after clone/pull |
| `depends` | `string[]` | none | Plugins that must be loaded before this one. Accepts `display_name` (e.g. `"snacks.nvim"`) or `url` (e.g. `"folke/snacks.nvim"`). **Eager plugin depending on a lazy plugin:** the lazy dep is auto-promoted to eager (a warning is printed to stderr). **Lazy plugin depending on a lazy plugin:** the dep(s) are loaded first inside the trigger callback via a `load_lazy` chain guarded against double-loading |
| `cond` | `string` | none | Lua expression. When set, the plugin's loader code is wrapped in `if <cond> then ... end` |
| `build` | `string` | none | Shell command to run after clone (not yet implemented) |
| `dev` | `bool` | `false` | When `true`, `sync` and `update` skip this plugin entirely (no clone/fetch/reset). Use for local development — the plugin stays on the rtp but rvpm won't touch the working tree |

</details>

### Lazy trigger fields

All trigger fields are optional. When multiple triggers are specified on the
same plugin they are OR-ed: **any one** firing loads the plugin.

<details>
<summary><b>All lazy triggers &amp; on_map formats</b></summary>

| Key | Type | Accepts | Description |
|---|---|---|---|
| `on_cmd` | `string \| string[]` | `"Foo"` or `["Foo", "Bar"]` | Load when the user runs `:Foo`. Supports bang, range, count, completion |
| `on_ft` | `string \| string[]` | `"rust"` or `["rust", "toml"]` | Load on `FileType` event, then re-trigger so `ftplugin/` fires |
| `on_event` | `string \| string[]` | `"BufReadPre"` or `["BufReadPre", "User LazyDone"]` | Load on Neovim event. `"User Xxx"` shorthand creates a User autocmd with `pattern = "Xxx"` |
| `on_path` | `string \| string[]` | `"*.rs"` or `["*.rs", "Cargo.toml"]` | Load on `BufRead` / `BufNewFile` matching the glob pattern |
| `on_source` | `string \| string[]` | `"snacks.nvim"` or `["snacks.nvim", "nui.nvim"]` | Load when the named plugin fires its `rvpm_loaded_<name>` User autocmd. Value must match the target plugin's `display_name` |
| `on_map` | `string \| MapSpec \| array` | see below | Load on keypress. Accepts simple `"<leader>f"` or table form |

#### `on_map` formats

```toml
# Simple string — normal mode, no desc
on_map = "<leader>f"

# Array of simple strings
on_map = ["<leader>f", "<leader>g"]

# Table form with mode and desc
on_map = [
  "<leader>f",
  { lhs = "<leader>v", mode = ["n", "x"] },
  { lhs = "<leader>g", mode = "n", desc = "Grep files" },
]
```

| MapSpec field | Type | Default | Description |
|---|---|---|---|
| `lhs` | `string` | **(required)** | The key sequence that triggers loading |
| `mode` | `string \| string[]` | `"n"` | Vim mode(s) for the keymap (`"n"`, `"x"`, `"i"`, etc.) |
| `desc` | `string` | none | Description shown in `:map` / which-key **before** the plugin is loaded |

</details>

### Colorscheme lazy loading

Lazy plugins that ship a `colors/` directory (containing `.vim` or `.lua`
files) are automatically given a `ColorSchemePre` autocmd handler at generate
time. No extra config field is required.

<details>
<summary><b>How it works &amp; example</b></summary>

When Neovim processes `:colorscheme <name>`, it fires `ColorSchemePre` before
switching the scheme. rvpm intercepts this event, loads the matching lazy
plugin, and then lets the colorscheme apply normally.

Eager plugins are unaffected: their `colors/` directory is already on the
runtimepath and Neovim finds it without any handler.

**Recommendation:** if you have multiple colorscheme plugins installed, mark
all but your active one as `lazy = true`. rvpm will register the
`ColorSchemePre` handler for each one so they remain switchable on demand
without adding startup cost.

```toml
[[plugins]]
url  = "folke/tokyonight.nvim"
lazy = true  # explicit — no on_* triggers to auto-infer from

[[plugins]]
url  = "catppuccin/nvim"
name = "catppuccin"
lazy = true  # explicit — ColorSchemePre is auto-registered, not an on_* field
```

> Colorscheme plugins don't have `on_*` triggers, so `lazy = true` must be
> written explicitly. rvpm handles the rest (scanning `colors/` and registering
> `ColorSchemePre`).

With this config, running `:colorscheme tokyonight` or `:colorscheme catppuccin`
in Neovim will load the respective plugin just in time, with zero startup
overhead when neither is the initial colorscheme.

</details>

### Hooks

Global hooks (`before.lua` / `after.lua` directly under `{config_root}/`)
and per-plugin hooks (under `{config_root}/plugins/<host>/<owner>/<repo>/`)
are auto-discovered — no config entries needed.

<details>
<summary><b>Hook file reference</b></summary>

#### Global hooks

Place Lua files directly under `{config_root}/` (default:
`~/.config/rvpm/<appname>/`) and rvpm picks them up automatically at
generate time:

| File | Phase | When it runs |
|---|---|---|
| `before.lua` | 3 | After `load_lazy` helper is defined, before any per-plugin `init.lua` |
| `after.lua`  | 9 | After all lazy trigger registrations |

Useful for setup that must happen before plugins are initialised
(e.g. setting `vim.g.*` globals) or post-load orchestration that doesn't
belong to any single plugin.

#### Per-plugin hooks

Drop Lua files under `{config_root}/plugins/<host>/<owner>/<repo>/` and
rvpm will include them in the generated loader:

| File | When it runs |
|---|---|
| `init.lua`   | Before `runtimepath` is touched (pre-rtp phase) |
| `before.lua` | Right after the plugin's rtp is added, before `plugin/*` is sourced |
| `after.lua`  | After `plugin/*` is sourced (safe to call plugin APIs) |

Example: `~/.config/rvpm/<appname>/plugins/github.com/nvim-telescope/telescope.nvim/after.lua`

```lua
require("telescope").setup({
  defaults = { layout_strategy = "vertical" },
})
vim.keymap.set("n", "<leader>ff", "<cmd>Telescope find_files<cr>")
```

</details>

## Commands

| Command | Description |
|---|---|
| `rvpm sync [--prune]` | Clone/pull plugins and regenerate `loader.lua`. `--prune` deletes unused plugin directories |
| `rvpm generate` | Regenerate `loader.lua` only (skip git operations) |
| `rvpm add <repo>` | Add a plugin and sync |
| `rvpm update [query]` | `git pull` installed plugins |
| `rvpm remove [query]` | Remove a plugin from `config.toml` and delete its directory |
| `rvpm edit [query] [--init\|--before\|--after] [--global]` | Edit per-plugin Lua config in `$EDITOR`. Flag skips the file picker. `--global` edits the global `before.lua` / `after.lua` |
| `rvpm set [query] [flags]` | Interactively or non-interactively tweak plugin options (lazy, merge, on\_\*, rev) |
| `rvpm config` | Open `config.toml` in `$EDITOR` |
| `rvpm init [--write]` | Print (or write) the `dofile(...)` snippet to wire `loader.lua` into `init.lua` |
| `rvpm list [--no-tui]` | TUI plugin list with action keys; `--no-tui` for pipe-friendly plain text |
| `rvpm store` | TUI plugin browser over the GitHub `neovim-plugin` topic; Enter to install, `o` to open in browser |

Run `rvpm <command> --help` for flag-level details.

### `rvpm list` — plugin manager TUI

<details>
<summary><b>Key bindings</b></summary>

| Key | Action |
|---|---|
| `j` / `k` / `↓` / `↑` | Move selection |
| `g` / `Home` | Go to top |
| `G` / `End` | Go to bottom |
| `Ctrl-d` / `Ctrl-u` | Half page down / up |
| `Ctrl-f` / `Ctrl-b` | Full page down / up |
| `/` | Incremental search |
| `n` / `N` | Next / previous search result |
| `e` | Edit per-plugin hooks (init / before / after.lua) |
| `s` | Set plugin options (lazy, merge, on_cmd, …) |
| `S` | Sync all plugins |
| `u` | Update selected plugin |
| `U` | Update all plugins |
| `d` | Remove selected plugin |
| `?` | Toggle help popup |
| `q` / `Esc` | Quit |

</details>

### `rvpm store` — plugin discovery TUI

Browse, search, and install plugins from GitHub without leaving the terminal.
`rvpm store` fetches up to ~300 repositories tagged with the `neovim-plugin`
topic, displays them in a split-pane TUI with a GitHub-flavored markdown
preview, and installs the selected plugin into your `config.toml` on `Enter`.

Plugins already listed in `config.toml` are marked with a green `✓` at the
start of the row; pressing `Enter` on an installed plugin shows a warning
instead of adding a duplicate.

<details>
<summary><b>Key bindings</b></summary>

Navigation keys are **focus-aware**: press `Tab` to switch between the
plugin list and the README preview pane.

| Key | List focused | README focused |
|---|---|---|
| `j` / `k` / `↓` / `↑` | Move selection | Scroll line |
| `g` / `Home` | Go to top | Scroll to top |
| `G` / `End` | Go to bottom | Scroll to bottom |
| `Ctrl-d` / `Ctrl-u` | Half page down / up | Half page scroll |
| `Ctrl-f` / `Ctrl-b` | Full page down / up | Full page scroll |

| Key | Action |
|---|---|
| `Tab` | Switch focus between list and README |
| `/` | Local incremental search over `name + description + topics` |
| `n` / `N` | Jump to next / previous search match |
| `S` | GitHub API search (`topic:neovim-plugin <query>`, replaces list) |
| `Enter` | Add the selected plugin to `config.toml` (warns if already installed) |
| `o` | Open the plugin's GitHub page in your default browser |
| `s` | Cycle sort mode (`stars` / `updated` / `name`) |
| `R` | Clear the search cache and re-fetch |
| `?` | Toggle help popup |
| `q` | Quit |
| `Esc` | Cancel active input (`/` or `S`) when in a search mode; quit otherwise |

**Legend:** `✓` in the leftmost column means the plugin is already in your
`config.toml`. Topics are shown in the rightmost column (`#lua #ui ...`).

</details>

**Caching:** search results are cached for 24 hours under
`{cache_root}/store/`; READMEs are cached for 7 days. Press `R` in the TUI
to force-refresh the search cache (README cache expires on its own TTL).

**Network requirement:** store needs network access to reach
`api.github.com` and `raw.githubusercontent.com`. Other commands
(`sync` / `update` / `generate` / `list` / ...) work offline once plugins
are cloned.

#### External README renderer

The built-in `tui-markdown` pipeline handles most READMEs reasonably well,
but it can't match dedicated renderers like `mdcat` or `glow` for tables,
task lists, or themed output. Configure an external command and rvpm will
pipe the raw README through it and render its ANSI output instead:

```toml
[options.store]
# Most common: mdcat reads from stdin by default
readme_command = ["mdcat"]

# Pass the terminal width explicitly
# readme_command = ["mdcat", "--columns", "{width}"]

# glow wants a file path and supports theme flags
# readme_command = ["glow", "-s", "dark", "-w", "{width}", "{file_path}"]

# bat can also pretty-print markdown
# readme_command = ["bat", "--language=markdown", "--color=always"]
```

**Placeholders** are expanded in every argument before spawning:

- `{width}` / `{height}` — inner size of the README pane in cells
- `{file_path}` — absolute path to a temp file containing the raw README
  (the command receives an empty stdin when this is used)
- `{file_dir}` — parent directory of `{file_path}`

**Contract and safeguards:**

- raw markdown goes to the command's **stdin** (unless `{file_path}` is used)
- the command's **stdout** is read and its ANSI escapes are parsed via
  `ansi-to-tui`, so any ANSI-aware renderer works
- hard timeout of 3 seconds per render; exceeding it falls back to the
  built-in path silently
- exit code ≠ 0, empty output, or spawn failure also falls back, with a
  one-line warning in the title bar
- leave `readme_command` unset to keep the offline built-in renderer as
  the default

<details>
<summary><b>More command examples</b></summary>

```sh
# ── Sync & generate ──────────────────────────────────────

# Clone/pull everything and regenerate loader.lua
rvpm sync

# Same, but also remove plugin dirs no longer in config.toml
rvpm sync --prune

# Only regenerate loader.lua (after editing init/before/after.lua)
rvpm generate

# ── Add / remove ─────────────────────────────────────────

# Add a plugin (creates entry in config.toml and syncs immediately)
rvpm add folke/snacks.nvim
rvpm add nvim-telescope/telescope.nvim --name telescope

# Remove interactively (fuzzy-select prompt)
rvpm remove

# Remove by name match
rvpm remove telescope

# ── Edit per-plugin hooks ────────────────────────────────

# Pick a plugin interactively, then pick which file to edit
rvpm edit

# Jump straight to a specific file (skips both selectors)
rvpm edit telescope --after
rvpm edit snacks --init
rvpm edit lspconfig --before

# ── Edit global hooks ────────────────────────────────────

# Open the interactive picker and select [ Global hooks ]
rvpm edit

# Jump straight to the global before/after hooks
rvpm edit --global --before    # phase 3
rvpm edit --global --after     # phase 9

# ── Set plugin options ───────────────────────────────────

# Interactive mode (fuzzy-select plugin → pick option → edit)
rvpm set

# Non-interactive: set multiple flags at once
rvpm set telescope --lazy true --on-cmd "Telescope"
rvpm set nvim-cmp --on-event '["InsertEnter", "CmdlineEnter"]'

# on_map with full JSON object form (mode + desc)
rvpm set which-key --on-map '{"lhs":"<leader>?","mode":["n","x"],"desc":"Which Key"}'

# Pin to a specific tag
rvpm set telescope --rev "0.1.8"

# ── Config / init ────────────────────────────────────────

# One-time setup: creates config.toml + init.lua in one shot
rvpm init --write

# Print the snippet without writing (dry run)
rvpm init

# Open config.toml in $EDITOR (auto-creates if missing; runs sync on close)
rvpm config

# ── List / status ────────────────────────────────────────

# TUI with interactive actions ([S] sync, [u] update, [d] remove, [?] help)
rvpm list

# Plain text for scripting / piping
rvpm list --no-tui
rvpm list --no-tui | grep Missing
```

</details>

## Design highlights

<details>
<summary><b>Loader model (9 phases)</b></summary>

```text
Phase 1: vim.go.loadplugins = false         -- disable Neovim's auto-source
Phase 2: load_lazy helper                   -- runtime loader for lazy plugins
Phase 3: global before.lua                  -- ~/.config/rvpm/<appname>/before.lua
Phase 4: all init.lua (dependency order)    -- pre-rtp phase
Phase 5: rtp:append(merged_dir)             -- once, if any merge=true plugins
Phase 6: eager plugins in dependency order:
             if not merge: rtp:append(plugin_path)
             before.lua
             source plugin/**/*.{vim,lua}    -- pre-globbed at generate time
             source ftdetect/** in augroup filetypedetect
             source after/plugin/**
             after.lua
             User autocmd "rvpm_loaded_<name>"
Phase 7: lazy trigger registrations         -- on_cmd / on_ft / on_map / etc
Phase 8: ColorSchemePre handlers            -- auto-registered for lazy plugins
                                              --   whose colors/ dir was detected at
                                              --   generate time (no config needed)
Phase 9: global after.lua                   -- ~/.config/rvpm/<appname>/after.lua
```

Because the file lists are baked in at `rvpm generate` time, the loader does
zero runtime glob work. `rvpm sync` (or `rvpm generate`) is what pays the I/O
cost; Neovim startup just sources a fixed list of files.

</details>

<details>
<summary><b>Merge optimization</b></summary>

When `merge = true`, the plugin directory is linked (junction on Windows,
symlink elsewhere) into `{cache_root}/plugins/merged/`. All `merge = true`
plugins share a single `vim.opt.rtp:append(merged_dir)` call, keeping
`&runtimepath` lean even with many eager plugins.

</details>

<details>
<summary><b>Dependency ordering &amp; lazy→eager promotion</b></summary>

`depends` fields are topologically sorted. Cycles and missing dependencies
emit warnings instead of hard-failing (resilience principle). The sort
ordering is preserved all the way through to the generated `loader.lua`, so
`before.lua` / `after.lua` hooks run in the correct order relative to
dependencies.

Beyond ordering, `depends` also affects **loading**:

- **Eager plugin → lazy dep**: a pre-pass during `generate_loader` detects
  this situation and auto-promotes the lazy dependency to eager, printing a
  note to stderr. This ensures the dep is unconditionally available before
  the eager plugin sources its files.
- **Lazy plugin → lazy dep**: the dependency is loaded on-demand. When the
  trigger fires, the generated callback calls `load_lazy` for each lazy dep
  (in dependency order) before loading the plugin itself. A double-load
  guard (`if loaded["<name>"] then return end`) prevents redundant sourcing
  when multiple plugins share the same dep and their triggers fire close
  together.

</details>

## Directory layout (defaults)

```text
~/.config/rvpm/<appname>/                    ← config_root
├── config.toml                              ← main configuration
├── before.lua                               ← global before hook (phase 3, auto-detected)
├── after.lua                                ← global after hook (phase 9, auto-detected)
└── plugins/
    └── <host>/<owner>/<repo>/
        ├── init.lua                         ← per-plugin hooks
        ├── before.lua
        └── after.lua

~/.cache/rvpm/<appname>/                     ← cache_root
├── plugins/
│   ├── repos/<host>/<owner>/<repo>/         ← plugin clones
│   ├── merged/                              ← linked root for merge=true plugins
│   └── loader.lua                           ← generated loader
└── store/                                   ← `rvpm store` cache (search + README)
```

Windows uses the same `.config` / `.cache` paths under `%USERPROFILE%` —
no `%APPDATA%` — to keep dotfiles portable between Linux / macOS / WSL /
Windows.

`<appname>` resolves to `$RVPM_APPNAME` → `$NVIM_APPNAME` → `"nvim"`.

## Development

```sh
# Build
cargo build

# Run the full test suite
cargo test

# Format check / lint
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings

# Inspect the generated loader from the sample fixture
cargo test dump_full_sample_loader -- --ignored --nocapture
```

rvpm is developed with **TDD**: tests come first, and new behaviors are
covered by either unit or integration tests before implementation.

## Acknowledgments

- **[lazy.nvim](https://github.com/folke/lazy.nvim)** — design inspiration
  for the plugin loading model and lazy trigger patterns.
- **[dvpm](https://github.com/yukimemi/dvpm)** — predecessor project (Deno-based).

## License

MIT — see [LICENSE](LICENSE).
