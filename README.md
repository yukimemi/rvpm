# rvpm

> **R**ust-based **V**im **P**lugin **M**anager — a fast, pre-compiled plugin manager for Neovim

[![CI](https://github.com/yukimemi/rvpm/actions/workflows/ci.yml/badge.svg)](https://github.com/yukimemi/rvpm/actions/workflows/ci.yml)
[![Release](https://github.com/yukimemi/rvpm/actions/workflows/release.yml/badge.svg)](https://github.com/yukimemi/rvpm/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

rvpm clones plugins in parallel, links `merge = true` plugins into a single
runtime-path entry, and ahead-of-time compiles a static `loader.lua` that
sources everything without any runtime glob cost.

## Demo

**`init` → `add` → `list` → `b` browse → `Enter` add → `l` back → `S` sync**

![rvpm](vhs/demo.gif)

## Why rvpm?

- **CLI-first** — manage plugins from your terminal, not from inside Neovim
- **TOML config with Tera templates** — declarative, conditional, and shareable across machines
- **Pre-compiled loader** — `rvpm generate` walks plugin directories at CLI
  time and bakes file lists into `loader.lua`; Neovim startup is a fixed
  list of `dofile()` / `source` calls with zero runtime glob
- **Full lazy-loading** — `on_cmd`, `on_ft`, `on_map`, `on_event`, `on_path`,
  `on_source`, plus auto-detected `ColorSchemePre` and `depends`-aware loading
- **File-level merge** — `merge = true` plugins share a single rtp entry via
  per-file hard links; namespace collisions surface in a `first-wins` summary
- **Plugin discovery TUI** — `rvpm browse` walks the GitHub `neovim-plugin`
  topic with live README preview; `Enter` to install
- **Diagnostics & history** — `rvpm doctor` reports config / state / env in one
  shot; `rvpm log` shows what commits landed on the last sync, with `⚠ BREAKING`
  highlight and optional inline `--diff`
- **Resilient** — cyclic dependencies, missing plugins, and config errors emit
  warnings, not crashes

## Installation

```sh
# From crates.io
cargo install rvpm

# Or from source (latest main)
cargo install --git https://github.com/yukimemi/rvpm
```

Pre-built binaries are also on the
[Releases](https://github.com/yukimemi/rvpm/releases) page for Linux
(x86_64), macOS (Intel / Apple Silicon), and Windows (x86_64). Extract the
binary into any directory on your `PATH`.

## Quick start

```sh
# 1. One-time setup — creates config.toml + wires loader into init.lua
rvpm init --write

# 2. Add plugins
rvpm add folke/snacks.nvim
rvpm add nvim-telescope/telescope.nvim

# 3. Browse the GitHub "neovim-plugin" topic and install from the TUI
rvpm browse

# 4. Manage installed plugins interactively
rvpm list

# 5. Open config.toml to tweak settings (lazy, triggers, etc.)
rvpm config
```

Files end up under `~/.config/rvpm/<appname>/` and
`~/.cache/rvpm/<appname>/` (see [Directory layout](#directory-layout)).
`<appname>` resolves to `$RVPM_APPNAME` → `$NVIM_APPNAME` → `"nvim"`.

## Configuration

`~/.config/rvpm/<appname>/config.toml`:

```toml
[vars]
# Your own variables, referenced via Tera templates {{ vars.xxx }}
nvim_rc = "~/.config/nvim/rc"

[options]
# Parallel git operations limit (default: 8)
concurrency = 10

# Auto-prune plugin dirs no longer referenced by config.toml on every
# sync / generate. Default: false. Equivalent to always passing --prune.
# auto_clean = true

# Auto-generate helptags via `nvim --headless` after sync / generate.
# Default: true. Set to false to skip.
# auto_helptags = false

# How `rvpm add` records GitHub plugin URLs in config.toml.
# "short" (default) → owner/repo ; "full" → https://github.com/owner/repo
# url_style = "full"

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

rvpm mirrors Neovim's `$NVIM_APPNAME` convention, so
`NVIM_APPNAME=nvim-test nvim` pairs with `NVIM_APPNAME=nvim-test rvpm sync`
for fully isolated test configs.

| Key | Type | Default | Description |
|---|---|---|---|
| `config_root` | `string` | `~/.config/rvpm/<appname>` | Root for `config.toml`, global hooks, and per-plugin hooks. **Recommended: leave unset** |
| `cache_root` | `string` | `~/.cache/rvpm/<appname>` | Root for clones, merged rtp, generated loader, and browse cache. **Recommended: leave unset** |
| `concurrency` | `integer` | `8` | Max parallel git operations during `sync` / `update` |
| `chezmoi` | `boolean` | `false` | Route writes through chezmoi source state. See [Advanced → chezmoi integration](#advanced) |
| `auto_clean` | `boolean` | `false` | `sync` / `generate` auto-delete plugin dirs no longer in `config.toml` (= always `--prune`) |
| `auto_helptags` | `boolean` | `true` | `sync` / `generate` run `nvim --headless` once at the end to build helptags for every plugin's `doc/`. Skipped with a warning if `nvim` is missing |
| `url_style` | `"short"` \| `"full"` | `"short"` | How `rvpm add` writes GitHub plugin URLs. Duplicate detection normalizes between styles |

> **💡 Leave `config_root` / `cache_root` unset.** Defaults are already
> `<appname>`-aware. Setting a literal path (e.g. `cache_root = "~/dotfiles/rvpm"`)
> breaks appname isolation — every `$NVIM_APPNAME` then shares the same
> cache. For a custom root *with* appname isolation, use a Tera template:
> `cache_root = "~/dotfiles/rvpm/{{ env.NVIM_APPNAME }}"`.

For everything beyond this minimal setup — full plugin field reference,
lazy trigger formats, hooks, conditional Tera templates, chezmoi
integration, and the external README renderer — see
[Advanced](#advanced).

## Commands

| Command | Description |
|---|---|
| `rvpm sync [--prune] [--frozen] [--no-lock] [--rebuild]` | Clone/pull plugins and regenerate `loader.lua`. `--prune` deletes unused plugin directories. `--frozen` errors out if any non-dev plugin is missing from `rvpm.lock` (CI reproducibility). `--no-lock` ignores the lockfile entirely. By default `build` commands are skipped when a pull is a no-op (saves time on configs with heavy `:TSUpdate`-style hooks); `--rebuild` forces every `build` to run regardless |
| `rvpm generate` | Regenerate `loader.lua` only (skip git operations) |
| `rvpm clean` | Delete plugin directories no longer referenced by `config.toml` (no git, faster than `sync --prune` on 200+ plugins) |
| `rvpm add <repo>` | Add a plugin and sync (records the new plugin's commit to `rvpm.lock`) |
| `rvpm update [query]` | `git pull` installed plugins and write new HEADs back to `rvpm.lock` |
| `rvpm remove [query]` | Remove a plugin from `config.toml` and delete its directory |
| `rvpm edit [query] [--init\|--before\|--after] [--global]` | Edit per-plugin Lua hooks in `$EDITOR`; `--global` for global hooks |
| `rvpm set [query] [flags]` | Tweak plugin options (`lazy`, `merge`, `on_*`, `rev`) interactively or via flags |
| `rvpm config` | Open `config.toml` in `$EDITOR` |
| `rvpm init [--write]` | Print (or write) the snippet to wire `loader.lua` into `init.lua` |
| `rvpm list [--no-tui]` | TUI plugin list with action keys; `--no-tui` for pipe-friendly plain text |
| `rvpm browse` | TUI plugin browser over the GitHub `neovim-plugin` topic |
| `rvpm doctor` | Diagnose config, state, Neovim wiring, and external tools. Exit codes: `0` ok / `1` error / `2` warn |
| `rvpm log [query] [--last N] [--full] [--diff]` | Show what commits landed on recent `sync` / `update` / `add`. `--diff` embeds README / CHANGELOG / `doc/` patches; `⚠ BREAKING` highlight for Conventional Commits |

Run `rvpm <command> --help` for flag-level details. TUI key bindings and
more example invocations are in [Advanced](#advanced).

## Directory layout

```text
~/.config/rvpm/<appname>/                    ← config_root
├── config.toml                              ← main configuration
├── rvpm.lock                                ← commit pins (commit alongside config.toml)
├── before.lua                               ← global before hook (phase 3)
├── after.lua                                ← global after hook (phase 9)
└── plugins/<host>/<owner>/<repo>/
    ├── init.lua                             ← per-plugin pre-rtp hook
    ├── before.lua                           ← per-plugin pre-source hook
    └── after.lua                            ← per-plugin post-source hook

~/.cache/rvpm/<appname>/                     ← cache_root
├── plugins/
│   ├── repos/<host>/<owner>/<repo>/         ← plugin clones
│   │   └── doc/tags                         ← helptags for lazy / merge=false plugins
│   ├── merged/                              ← hard-linked rtp for merge=true
│   │   └── doc/tags                         ← helptags shared across merged plugins
│   └── loader.lua                           ← generated loader
├── browse/                                  ← `rvpm browse` cache (search + README)
├── update_log.json                          ← `rvpm log` history (last 20 runs)
└── merge_conflicts.json                     ← last sync's merge conflicts (read by `rvpm doctor`)
```

Windows uses the same `.config` / `.cache` paths under `%USERPROFILE%`
(no `%APPDATA%`), so the same layout is portable across Linux / macOS /
WSL / Windows.

## Advanced

Everything below is folded by default — open only the topics relevant to
what you're doing.

<details>
<summary><b>Plugin spec — all <code>[[plugins]]</code> fields</b></summary>

| Key | Type | Default | Description |
|---|---|---|---|
| `url` | `string` | **(required)** | Plugin repository. `owner/repo` (GitHub shorthand), full URL, or local path |
| `name` | `string` | repo name from `url` (e.g. `telescope.nvim`) | Friendly name used in `rvpm_loaded_<name>` User autocmd, `on_source` chain, and log messages. Auto-derived by taking the last path component of the URL and stripping `.git` |
| `dst` | `string` | `{cache_root}/plugins/repos/<host>/<owner>/<repo>` | Custom clone destination (overrides the default path layout) |
| `lazy` | `bool` | auto | **Auto-inferred**: if any `on_*` trigger is set, defaults to `true`; otherwise `false`. Write `lazy = false` explicitly to force eager loading even with triggers |
| `merge` | `bool` | `true` | If `true`, the plugin's runtime files are hard-linked into `{cache_root}/plugins/merged/` and share a single runtimepath entry |
| `rev` | `string` | HEAD | Branch, tag, or commit hash to check out after clone/pull |
| `depends` | `string[]` | none | Plugins that must be loaded before this one. Accepts `display_name` (e.g. `"snacks.nvim"`) or `url` (e.g. `"folke/snacks.nvim"`). Eager → lazy dep auto-promotes the dep to eager (with a warning); lazy → lazy dep loads dep first inside the trigger callback |
| `cond` | `string` | none | Lua expression. When set, the plugin's loader code is wrapped in `if <cond> then ... end` |
| `build` | `string` | none | Shell command to run after clone (not yet implemented) |
| `dev` | `bool` | `false` | When `true`, `sync` and `update` skip this plugin entirely (no clone/fetch/reset). Use for local development |

</details>

<details>
<summary><b>Lazy triggers — <code>on_cmd</code> / <code>on_ft</code> / <code>on_map</code> / <code>on_event</code> / <code>on_path</code> / <code>on_source</code></b></summary>

All trigger fields are optional. When multiple triggers are set on the
same plugin they are OR-ed: **any one** firing loads the plugin.

| Key | Type | Accepts | Description |
|---|---|---|---|
| `on_cmd` | `string \| string[]` | `"Foo"` or `["Foo", "Bar"]` | Load when the user runs `:Foo`. Supports bang, range, count, completion |
| `on_ft` | `string \| string[]` | `"rust"` or `["rust", "toml"]` | Load on `FileType`, then re-trigger so `ftplugin/` fires |
| `on_event` | `string \| string[]` | `"BufReadPre"` or `["BufReadPre", "User LazyDone"]` | Load on Neovim event. `"User Xxx"` shorthand creates a User autocmd with `pattern = "Xxx"` |
| `on_path` | `string \| string[]` | `"*.rs"` or `["*.rs", "Cargo.toml"]` | Load on `BufRead` / `BufNewFile` matching the glob pattern |
| `on_source` | `string \| string[]` | `"snacks.nvim"` or `["snacks.nvim", "nui.nvim"]` | Load when the named plugin fires its `rvpm_loaded_<name>` User autocmd |
| `on_map` | `string \| MapSpec \| array` | see below | Load on keypress. Simple `"<leader>f"` or table form |

**`on_map` formats:**

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
| `lhs` | `string` | **(required)** | Key sequence that triggers loading |
| `mode` | `string \| string[]` | `"n"` | Vim mode(s) for the keymap (`"n"`, `"x"`, `"i"`, etc.) |
| `desc` | `string` | none | Description shown in `:map` / which-key **before** the plugin is loaded |

</details>

<details>
<summary><b>Hooks — global & per-plugin Lua files</b></summary>

Global hooks (`before.lua` / `after.lua` directly under `{config_root}/`)
and per-plugin hooks (under `{config_root}/plugins/<host>/<owner>/<repo>/`)
are auto-discovered — no config entries needed.

**Global hooks** — place Lua files directly under `{config_root}/`
(default: `~/.config/rvpm/<appname>/`):

| File | Phase | When it runs |
|---|---|---|
| `before.lua` | 3 | After `load_lazy` helper is defined, before any per-plugin `init.lua` |
| `after.lua`  | 9 | After all lazy trigger registrations |

Useful for setup that must happen before plugins are initialised
(e.g. `vim.g.*` globals) or post-load orchestration that doesn't belong
to any single plugin.

**Per-plugin hooks** — drop Lua files under
`{config_root}/plugins/<host>/<owner>/<repo>/`:

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

<details>
<summary><b>Colorscheme lazy loading</b></summary>

Lazy plugins that ship a `colors/` directory (containing `.vim` or `.lua`
files) automatically gain a `ColorSchemePre` autocmd handler at generate
time. No extra config field is required.

When Neovim processes `:colorscheme <name>`, it fires `ColorSchemePre`
before switching the scheme. rvpm intercepts this event, loads the
matching lazy plugin, then lets the colorscheme apply normally.

Eager plugins are unaffected: their `colors/` directory is already on
the runtimepath and Neovim finds it without any handler.

**Recommendation:** if you have multiple colorscheme plugins, mark all
but your active one as `lazy = true`. rvpm registers the
`ColorSchemePre` handler for each so they remain switchable on demand
without adding startup cost.

```toml
[[plugins]]
url  = "folke/tokyonight.nvim"
lazy = true  # explicit — no on_* triggers to auto-infer from

[[plugins]]
url  = "catppuccin/nvim"
name = "catppuccin"
lazy = true
```

> Colorscheme plugins don't have `on_*` triggers, so `lazy = true` must
> be written explicitly. rvpm handles the rest (scanning `colors/` and
> registering `ColorSchemePre`).

</details>

<details>
<summary><b>Tera templates — vars, env, conditionals</b></summary>

The entire `config.toml` is processed by
[Tera](https://keats.github.io/tera/) before TOML parsing. You can use
`{{ vars.xxx }}`, `{{ env.HOME }}`, `{{ is_windows }}`, `{% if %}` blocks,
and more.

**Available context:**

| Variable | Type | Description |
|---|---|---|
| `vars.*` | any | User-defined variables from `[vars]` |
| `env.*` | string | Environment variables (e.g. `{{ env.HOME }}`) |
| `is_windows` | bool | `true` on Windows, `false` otherwise |

**Variables referencing other variables** — including forward references:

```toml
[vars]
base = "~/.cache/rvpm"
full = "{{ vars.base }}/custom"   # → "~/.cache/rvpm/custom"

# Forward reference works too
greeting = "Hello {{ vars.name }}"
name = "yukimemi"
# greeting → "Hello yukimemi"
```

**Conditional plugin inclusion** — `{% if %}` excludes plugins from
`loader.lua` entirely at generate time:

```toml
[vars]
use_blink = true
use_cmp = false

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
```

**Platform-specific plugins:**

```toml
{% if is_windows %}
[[plugins]]
url = "thinca/vim-winenv"
{% endif %}

[[plugins]]
url = "folke/snacks.nvim"
cond = "{{ is_windows }}"  # runtime cond: kept in loader but guarded
```

> **`{% if %}` vs `cond`**: `{% if %}` removes the plugin entirely at
> generate time — no clone, no merge, not in `loader.lua`. `cond` keeps
> the plugin in `loader.lua` but wraps it in `if <expr> then ... end`
> for runtime evaluation.

</details>

<details>
<summary><b>chezmoi integration</b></summary>

If you manage your dotfiles with [chezmoi](https://www.chezmoi.io/), set
`chezmoi = true` and rvpm routes every write through the chezmoi
**source state** instead of mutating the target file directly —
preserving chezmoi's "source is truth" model:

```toml
[options]
chezmoi = true
```

Every mutation that touches `config.toml`, a global hook, or a
per-plugin hook (`rvpm add` / `set` / `remove` / `edit` / `config` /
`init --write`, plus the `e` / `s` / `d` action keys in `rvpm list`)
follows this flow:

1. **Resolve the source path.** rvpm asks chezmoi via
   `chezmoi source-path <target>`. If the target itself isn't managed,
   rvpm walks its ancestors until it hits a managed directory. This is
   how newly created per-plugin hook files under a managed
   `plugins/<host>/<owner>/<repo>/` parent get picked up.
2. **Write to the source file.** rvpm writes the new content into the
   resolved source path. The target file is not touched at this step.
3. **Apply back.** rvpm runs `chezmoi apply --force <target>` to
   materialise the change. `--force` is intentional — rvpm is the
   authoritative writer of these files.

Files whose ancestors aren't managed by chezmoi are left alone, so
enabling the flag is safe even when only part of your rvpm tree lives
in chezmoi.

**Limitations:**

- `.tmpl` sources are rejected. rvpm has its own Tera engine; writing
  into a `.tmpl` would silently corrupt the chezmoi template. Falls
  back to writing the target directly with a warning.
- If `chezmoi` is missing from `PATH`, rvpm warns loudly and writes to
  the target directly. The primary operation always succeeds.

</details>

<details>
<summary><b>External README renderer for <code>rvpm browse</code></b></summary>

The built-in `tui-markdown` pipeline handles most READMEs reasonably,
but can't match dedicated renderers like `mdcat` or `glow` for tables,
task lists, or themed output. Configure an external command and rvpm
pipes the raw README through it, rendering the ANSI output:

```toml
[options.browse]
# Most common: mdcat reads from stdin by default
readme_command = ["mdcat"]

# Pass terminal width explicitly (Tera-style `{{ name }}` placeholders)
# readme_command = ["mdcat", "--columns", "{{ width }}"]

# glow wants a file path
# readme_command = ["glow", "-s", "dark", "-w", "{{ width }}", "{{ file_path }}"]

# bat can also pretty-print markdown
# readme_command = ["bat", "--language=markdown", "--color=always"]
```

**Placeholders** use the same `{{ name }}` syntax as elsewhere
(whitespace optional). Unknown names are left literal:

- `{{ width }}` / `{{ height }}` — inner size of the README pane in cells
- `{{ file_path }}` — absolute path to a temp file containing the raw
  README (the command receives empty stdin when any `{{ file_* }}` is used)
- `{{ file_dir }}` / `{{ file_name }}` / `{{ file_stem }}` / `{{ file_ext }}`

**Contract & safeguards:**

- raw markdown goes to stdin (unless `{{ file_path }}` is used)
- stdout is read and ANSI escapes parsed via `ansi-to-tui`
- 3-second hard timeout per render; exceeding falls back silently
- exit code ≠ 0, empty output, or spawn failure also falls back, with a
  one-line warning in the title bar
- leave `readme_command` unset to keep the offline built-in renderer

</details>

<details>
<summary><b><code>rvpm list</code> — TUI key bindings</b></summary>

| Key | Action |
|---|---|
| `j` / `k` / `↓` / `↑` | Move selection |
| `g` / `Home` | Go to top |
| `G` / `End` | Go to bottom |
| `Ctrl-d` / `Ctrl-u` | Half page down / up |
| `Ctrl-f` / `Ctrl-b` | Full page down / up |
| `/` | Incremental search |
| `n` / `N` | Next / previous search result |
| `b` | Switch to `rvpm browse` TUI |
| `c` | Open `config.toml` in `$EDITOR` |
| `e` | Edit per-plugin hooks (init / before / after.lua) |
| `s` | Set plugin options (lazy, merge, on_cmd, …) |
| `S` | Sync all plugins |
| `R` | Sync all plugins with `--rebuild` (force-run every `build` command, even no-op pulls) |
| `u` | Update selected plugin |
| `U` | Update all plugins |
| `d` | Remove selected plugin |
| `?` | Toggle help popup |
| `q` / `Esc` | Quit |

</details>

<details>
<summary><b><code>rvpm browse</code> — TUI key bindings & caching</b></summary>

`rvpm browse` fetches up to ~300 repositories tagged with the
`neovim-plugin` topic, displays them in a split-pane TUI with a
GitHub-flavored markdown preview, and installs the selected plugin into
your `config.toml` on `Enter`. Plugins already in `config.toml` are
marked with a green `✓`.

Navigation keys are **focus-aware** — press `Tab` to switch panes:

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
| `l` | Switch to `rvpm list` TUI |
| `c` | Open `config.toml` in `$EDITOR` |
| `o` | Open the plugin's GitHub page in your default browser |
| `s` | Cycle sort mode (`stars` / `updated` / `name`) |
| `R` | Clear the search cache and re-fetch |
| `?` | Toggle help popup |
| `q` | Quit |
| `Esc` | Cancel active input (`/` or `S`); quit otherwise |

**Caching:** search results are cached for 24 hours under
`{cache_root}/browse/`; READMEs for 7 days. Press `R` to force-refresh
the search cache.

**Network:** browse needs network access to `api.github.com` and
`raw.githubusercontent.com`. Other commands work offline once plugins
are cloned.

</details>

<details>
<summary><b>Loader model — 9 phases</b></summary>

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

Because file lists are baked in at `rvpm generate` time, the loader does
zero runtime glob work. `rvpm sync` (or `rvpm generate`) pays the I/O
cost; Neovim startup just sources a fixed list of files.

</details>

<details>
<summary><b>Lockfile — reproducible plugin versions</b></summary>

`rvpm sync` writes `<config_root>/rvpm.lock` alongside `config.toml`,
recording the resolved commit hash of every installed plugin. Commit it
to your dotfiles and any machine running `rvpm sync` reproduces the
exact plugin set — the same workflow `lazy.nvim` provides via
`lazy-lock.json`.

```toml
# rvpm.lock — generated by rvpm. Commit this alongside config.toml for reproducibility.
# Do not edit by hand; run `rvpm sync` or `rvpm update` to refresh.

version = 1

[[plugins]]
name = "snacks.nvim"
url = "folke/snacks.nvim"
commit = "abc123def456..."
```

**Priority per plugin:** `rev` (explicit, in `config.toml`) > lockfile
commit > branch HEAD. So `rev = "v1.2.3"` always wins; the lockfile
fills in for plugins without an explicit pin; the old "pull latest"
behaviour only kicks in when neither is set. `dev = true` plugins are
excluded (pinning local work-in-progress doesn't make sense).

**Command interactions:**

| Command | Lockfile behaviour |
|---|---|
| `rvpm sync` | Read `rvpm.lock` → check out locked commits → write new HEADs back → drop entries for plugins removed from `config.toml` |
| `rvpm sync --frozen` | Bail before syncing if any non-dev plugin is missing from `rvpm.lock` **or** has a URL mismatch (stale entry). For CI / fresh clones that require strict reproducibility |
| `rvpm sync --no-lock` | Skip `rvpm.lock` entirely. Existing file on disk is untouched |
| `rvpm update [query]` | Always pull latest, then upsert new HEADs. Partial updates preserve untouched entries |
| `rvpm add <repo>` | Write the freshly-installed plugin's HEAD into `rvpm.lock` |

The `--frozen` check also catches URL changes: if `config.toml` points
at `owner/foo.nvim` but the lockfile entry's `url` is for a different
repo, the run errors out cleanly instead of checking out a stale commit
against the wrong repository.

`--frozen` and `--no-lock` together is a contradiction (one demands
the lockfile, the other ignores it) and is rejected up-front so CI
operators notice the mistake.

</details>

<details>
<summary><b>Merge strategy — file-level hard links</b></summary>

When `merge = true`, the plugin's runtime files are **hard-linked at
the file level** into `{cache_root}/plugins/merged/`. All `merge = true`
plugins share a single `vim.opt.rtp:append(merged_dir)` call, keeping
`&runtimepath` lean even with many eager plugins.

File-level linking matters when multiple plugins place files under the
same directory (e.g., several cmp-related plugins dropping files into
`lua/cmp/`). The naive directory-link approach loses the later plugin's
contents; rvpm walks each plugin recursively and hard-links individual
files, surfacing any path collision in a `first-wins` summary at the
end of `sync` / `generate`.

Hard links work on Windows without admin rights (unlike symbolic links)
and on every Unix; they only require the source and target to be on the
same volume — and rvpm keeps both `repos/` and `merged/` under
`<cache_root>` for that reason. If a hard link fails (cross-volume,
non-NTFS quirks), the link falls back to a copy.

Plugin-root metadata (`README.md`, `LICENSE`, `Makefile`, `*.toml`,
`package.json`, etc.) is skipped — it's not on any runtimepath and
just adds noise. Dotfiles at any depth (`.gitignore`, `.luarc.json`)
are skipped for the same reason. Only the standard rtp directories
(`plugin/`, `lua/`, `doc/`, `ftplugin/`, `colors/`, `queries/`,
`tutor/`, …) plus `denops/` (for
[denops.vim](https://github.com/vim-denops/denops.vim) TypeScript
plugins) are walked.

</details>

<details>
<summary><b>Dependency ordering &amp; lazy→eager promotion</b></summary>

`depends` fields are topologically sorted. Cycles and missing
dependencies emit warnings instead of hard-failing (resilience
principle). Sort order is preserved through to the generated
`loader.lua`, so `before.lua` / `after.lua` hooks run in the correct
order relative to dependencies.

Beyond ordering, `depends` also affects **loading**:

- **Eager plugin → lazy dep**: a pre-pass during `generate_loader`
  detects this and auto-promotes the lazy dependency to eager,
  printing a note to stderr. This ensures the dep is unconditionally
  available before the eager plugin sources its files.
- **Lazy plugin → lazy dep**: the dependency is loaded on-demand. When
  the trigger fires, the generated callback calls `load_lazy` for each
  lazy dep (in dependency order) before loading the plugin itself. A
  double-load guard (`if _G["rvpm_loaded_" .. name] then return end`)
  prevents redundant sourcing when multiple plugins share the same dep.

</details>

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

# ── Diagnostics & history ────────────────────────────────

# Diagnose config / state / Neovim wiring / external tools
rvpm doctor

# Show what commits landed on the last sync / update
rvpm log

# Last 5 runs, with README/CHANGELOG/doc patches inline
rvpm log --last 5 --diff

# Filter by plugin name substring
rvpm log telescope

# ── List ─────────────────────────────────────────────────

# TUI with interactive actions
rvpm list

# Plain text for scripting / piping
rvpm list --no-tui
rvpm list --no-tui | grep Missing
```

</details>

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
