# rvpm

> **R**ust-based **V**im **P**lugin **M**anager — a fast, pre-compiled plugin manager for Neovim

[![CI](https://github.com/yukimemi/rvpm/actions/workflows/ci.yml/badge.svg)](https://github.com/yukimemi/rvpm/actions/workflows/ci.yml)
[![Release](https://github.com/yukimemi/rvpm/actions/workflows/release.yml/badge.svg)](https://github.com/yukimemi/rvpm/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

rvpm clones plugins in parallel, links `merge = true` plugins into a single
runtime-path entry, and ahead-of-time compiles a `loader.lua` that sources
everything without any runtime `vim.fn.glob` cost.

Inspired by [lazy.nvim](https://github.com/folke/lazy.nvim) — rvpm adopts the
same "take full control of plugin loading" approach (`vim.go.loadplugins =
false`), but adds **merge optimization** and **generate-time file-list
compilation** on top.

## Why rvpm?

| | lazy.nvim | rvpm |
|---|---|---|
| Plugin loading control | ✓ (own dispatch) | ✓ (own dispatch) |
| `init` / `config` hooks | ✓ | ✓ (`init.lua` / `before.lua` / `after.lua`) |
| Per-plugin runtimepath | ✓ | ✓ (when `merge = false`) |
| **Merged runtimepath** (single rtp entry for many plugins) | ✗ | ✓ |
| **Runtime glob elimination** (plugin file paths baked at generate time) | ✗ | ✓ |
| Written in | Lua | Rust |
| Installation workflow | Lua in `init.lua` | CLI tool, static `loader.lua` |
| Parallel git operations | Lua coroutines | Tokio `JoinSet` + `Semaphore` |
| Config format | Lua tables | TOML + Tera templates |

The upshot: rvpm does more work at `rvpm sync` / `rvpm generate` time so that
Neovim startup reads exactly the files it needs and nothing else.

## Features

- **Fast startup** — Phase 0–4 loader model with `vim.go.loadplugins = false`
  and pre-globbed `plugin/` / `ftdetect/` / `after/plugin/` file lists
- **Global hooks** — `~/.config/rvpm/before.lua` (Phase 0.7, before all plugin
  `init.lua`) and `~/.config/rvpm/after.lua` (Phase 4.5, after all lazy trigger
  registrations); auto-detected at generate time, no config required
- **Merge optimization** — `merge = true` plugins share a single
  `vim.opt.rtp:append(...)` entry via junction/symlink
- **Full lazy triggers** — `on_cmd` / `on_ft` / `on_map` / `on_event` /
  `on_path` / `on_source` (plugin chain), with `User Xxx` pattern shorthand,
  bang/range/count/complete aware commands, keymaps with mode + desc, and
  `<Ignore>`-prefixed replay for safety
- **Dependency ordering** — topological sort on `depends`, resilient to cycles
  and missing references
- **Windows first-class** — hardcoded `~/.config` / `~/.cache` layout for
  dotfiles portability, junction instead of symlink to avoid permission
  issues
- **Interactive TUI** (`rvpm list`) — plugin list with action keys for
  sync/update/generate/remove/edit/set
- **CLI-driven set** — `rvpm set foo --on-event '["BufReadPre","User Started"]'`
  or full JSON object form for on_map with mode/desc
- **TOML direct edit escape hatch** — `rvpm config` / `rvpm set` sub-menu to
  jump to the plugin's block in `$EDITOR`
- **Init.lua integration** — `rvpm init --write` wires the generated loader
  into `~/.config/$NVIM_APPNAME/init.lua` (creates the file if missing)

## Installation

### From a pre-built binary

Download the latest archive from the
[Releases](https://github.com/yukimemi/rvpm/releases) page for your platform:

- **Linux (x86_64)**: `rvpm-x86_64-unknown-linux-gnu.tar.gz`
- **macOS (Intel)**: `rvpm-x86_64-apple-darwin.tar.gz`
- **macOS (Apple Silicon)**: `rvpm-aarch64-apple-darwin.tar.gz`
- **Windows (x86_64)**: `rvpm-x86_64-pc-windows-msvc.zip`

Extract the binary into any directory on your `PATH`.

### From crates.io

```sh
cargo install rvpm
```

### From source (latest main)

```sh
cargo install --git https://github.com/yukimemi/rvpm
```

## Quick start

```sh
# 1. One-time setup (creates both config.toml and init.lua)
rvpm init --write
# → ~/.config/rvpm/config.toml  (plugin configuration, auto-created)
# → ~/.config/nvim/init.lua     (loader wiring, auto-created or appended)
# Respects $NVIM_APPNAME for custom Neovim configs.

# 2. Add plugins
rvpm add folke/snacks.nvim
rvpm add nvim-telescope/telescope.nvim

# 3. Open config.toml to tweak settings (lazy, triggers, etc.)
rvpm config

# 4. Explore the TUI
rvpm list
```

## Configuration

`~/.config/rvpm/config.toml`:

```toml
[vars]
# Your own variables, referenced via Tera templates {{ vars.xxx }}
nvim_rc = "~/.config/nvim/rc"

[options]
# Per-plugin init/before/after.lua directory
# Default: ~/.config/rvpm/plugins
config_root = "{{ vars.nvim_rc }}/plugins"
# Parallel git operations limit (default: 8)
concurrency = 10
# Optional: move all rvpm data (repos + merged + loader.lua) under a custom root
# base_dir = "~/dotfiles/nvim/rvpm"
# Optional: override only loader.lua location (overrides base_dir for loader)
# loader_path = "~/.cache/nvim/rvpm/loader.lua"

[[plugins]]
name  = "snacks"
url   = "folke/snacks.nvim"
merge = true     # Default for eager plugins
lazy  = false

[[plugins]]
name    = "telescope"
url     = "nvim-telescope/telescope.nvim"
lazy    = true
depends = ["snacks.nvim"]
# Trigger on command — plugin loads when the user runs :Telescope
on_cmd  = ["Telescope"]
# Or as a User autocmd chained off another plugin
on_source = ["snacks.nvim"]

[[plugins]]
url     = "neovim/nvim-lspconfig"
lazy    = true
# Multiple triggers are OR-ed: any one firing loads the plugin
on_ft   = ["rust", "toml", "lua"]
on_event = ["BufReadPre", "User LazyVimStarted"]

[[plugins]]
name = "which-key"
url  = "folke/which-key.nvim"
lazy = true
# on_map accepts simple strings or full `{ lhs, mode, desc }` tables
on_map = [
  "<leader>?",
  { lhs = "<leader>v", mode = ["n", "x"], desc = "Visual leader" },
]
```

### `[options]` reference

| Key | Type | Default | Description |
|---|---|---|---|
| `config_root` | `string` | `~/.config/rvpm/plugins` | Root directory for per-plugin `init.lua` / `before.lua` / `after.lua`. Supports `~` and `{{ vars.xxx }}` templates |
| `concurrency` | `integer` | `8` | Max number of parallel git operations during `sync` / `update`. Kept moderate to avoid GitHub rate limits |
| `base_dir` | `string` | `~/.cache/rvpm` | Root for all rvpm data (repos, merged, loader.lua). Setting this moves everything together |
| `loader_path` | `string` | `{base_dir}/loader.lua` | Override only the loader.lua output path. Takes precedence over `base_dir` for the loader file |

### `[[plugins]]` reference

| Key | Type | Default | Description |
|---|---|---|---|
| `url` | `string` | **(required)** | Plugin repository. `owner/repo` (GitHub shorthand), full URL, or local path |
| `name` | `string` | repo name from `url` (e.g. `telescope.nvim`) | Friendly name used in `rvpm_loaded_<name>` User autocmd, `on_source` chain, and log messages. Auto-derived by taking the last path component of the URL and stripping `.git` |
| `dst` | `string` | `{base_dir}/repos/<host>/<owner>/<repo>` | Custom clone destination (overrides the default path layout) |
| `lazy` | `bool` | `false` | If `true`, the plugin is not loaded at startup — requires at least one trigger (`on_cmd`, `on_ft`, etc.) |
| `merge` | `bool` | `true` | If `true`, the plugin directory is linked into `{base_dir}/merged/` and shares a single runtimepath entry |
| `rev` | `string` | HEAD | Branch, tag, or commit hash to check out after clone/pull |
| `depends` | `string[]` | none | Plugins that must be loaded first. Accepts `display_name` (e.g. `"snacks.nvim"`) or `url` (e.g. `"folke/snacks.nvim"`) |
| `cond` | `string` | none | Lua expression. When set, the plugin's loader code is wrapped in `if <cond> then ... end` |
| `build` | `string` | none | Shell command to run after clone (not yet implemented) |

### Lazy trigger fields

All trigger fields are optional. When multiple triggers are specified on the same plugin they are OR-ed: **any one** firing loads the plugin.

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

### Global hooks

Place Lua files directly under `~/.config/rvpm/` and rvpm picks them up
automatically at generate time — no configuration entry needed:

| File | Phase | When it runs |
|---|---|---|
| `~/.config/rvpm/before.lua` | 0.7 | After `load_lazy` helper is defined, before any per-plugin `init.lua` |
| `~/.config/rvpm/after.lua` | 4.5 | After all lazy trigger registrations |

These are useful for any setup that must happen before plugins are initialised
(e.g. setting `vim.g.*` globals) or for post-load orchestration that doesn't
belong to any single plugin.

### Per-plugin hooks

Drop Lua files under `{config_root}/<host>/<owner>/<repo>/` and rvpm will
include them in the generated loader:

| File | When it runs |
|---|---|
| `init.lua` | Before `runtimepath` is touched (pre-rtp phase) |
| `before.lua` | Right after the plugin's rtp is added, before `plugin/*` is sourced |
| `after.lua` | After `plugin/*` is sourced (safe to call plugin APIs) |

Example: `~/.config/rvpm/plugins/github.com/nvim-telescope/telescope.nvim/after.lua`

```lua
require("telescope").setup({
  defaults = { layout_strategy = "vertical" },
})
vim.keymap.set("n", "<leader>ff", "<cmd>Telescope find_files<cr>")
```

## Commands

| Command | Description |
|---|---|
| `rvpm sync [--prune]` | Clone/pull plugins and regenerate `loader.lua`. `--prune` deletes unused plugin directories |
| `rvpm generate` | Regenerate `loader.lua` only (skip git operations) |
| `rvpm add <repo>` | Add a plugin and sync |
| `rvpm update [query]` | `git pull` installed plugins |
| `rvpm remove [query]` | Remove a plugin from `config.toml` and delete its directory |
| `rvpm edit [query] [--init\|--before\|--after] [--global]` | Edit per-plugin Lua config in `$EDITOR`. Flag skips the file picker. `--global` (or selecting `[ Global hooks ]` in the interactive picker) edits `~/.config/rvpm/before.lua` / `after.lua` |
| `rvpm set [query] [flags]` | Interactively or non-interactively tweak plugin options (lazy, merge, on\_\*, rev) |
| `rvpm config` | Open `config.toml` in `$EDITOR` |
| `rvpm init [--write]` | Print (or write) the `dofile(...)` snippet to wire `loader.lua` into `init.lua` |
| `rvpm list [--no-tui]` | TUI plugin list with action keys; `--no-tui` for pipe-friendly plain text |

Run `rvpm <command> --help` for flag-level details.

### Usage examples

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
rvpm edit --global --before    # ~/.config/rvpm/before.lua (Phase 0.7)
rvpm edit --global --after     # ~/.config/rvpm/after.lua  (Phase 4.5)

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

# Drop into $EDITOR for manual TOML editing from the set menu
# → pick a plugin → select [ Open config.toml in $EDITOR ]

# ── Config / init ────────────────────────────────────────

# One-time setup: creates config.toml + init.lua in one shot
rvpm init --write

# Print the snippet without writing (dry run)
rvpm init

# Open config.toml in $EDITOR (auto-creates if missing; runs sync on close)
rvpm config

# ── List / status ────────────────────────────────────────

# TUI with interactive actions ([S] sync, [u] update, [d] remove, …)
rvpm list

# Plain text for scripting / piping
rvpm list --no-tui
rvpm list --no-tui | grep Missing
```

## Design highlights

### Phase 0–4 loader model

```
Phase 0:   vim.go.loadplugins = false         -- disable Neovim's auto-source
Phase 0.5: load_lazy helper                   -- runtime loader for lazy plugins
Phase 0.7: global before.lua                  -- ~/.config/rvpm/before.lua (if present)
Phase 1:   all init.lua (dependency order)   -- pre-rtp phase
Phase 2:   rtp:append(merged_dir)             -- once, if any merge=true plugins
Phase 3:   eager plugins in dependency order:
             if not merge: rtp:append(plugin_path)
             before.lua
             source plugin/**/*.{vim,lua}    -- pre-globbed at generate time
             source ftdetect/** in augroup filetypedetect
             source after/plugin/**
             after.lua
             User autocmd "rvpm_loaded_<name>"
Phase 4:   lazy trigger registrations (on_cmd / on_ft / on_map / etc)
Phase 4.5: global after.lua                  -- ~/.config/rvpm/after.lua (if present)
```

Because the file lists are baked in at `rvpm generate` time, the loader does
zero runtime glob work. `rvpm sync` (or `rvpm generate`) is what pays the I/O
cost; Neovim startup just sources a fixed list of files.

### Merge optimization

When `merge = true`, the plugin directory is linked (junction on Windows,
symlink elsewhere) into `{base_dir}/merged/`. All `merge = true` plugins share
a single `vim.opt.rtp:append(merged_dir)` call — lazy.nvim doesn't do this, so
if you have ~100 eager plugins, rvpm keeps your `&runtimepath` lean.

### Dependency ordering

`depends` fields are topologically sorted. Cycles and missing dependencies
emit warnings instead of hard-failing (resilience principle). The sort
ordering is preserved all the way through to the generated `loader.lua`, so
`before.lua` / `after.lua` hooks run in the correct order relative to
dependencies.

## Directory layout (defaults)

| Path | Purpose |
|---|---|
| `~/.config/rvpm/config.toml` | Main configuration (fixed location) |
| `~/.config/rvpm/before.lua` | Global before hook — runs at Phase 0.7, before all plugin `init.lua` |
| `~/.config/rvpm/after.lua` | Global after hook — runs at Phase 4.5, after all lazy trigger registrations |
| `~/.config/rvpm/plugins/<host>/<owner>/<repo>/` | Per-plugin `init/before/after.lua` (`options.config_root` to override) |
| `~/.cache/rvpm/repos/<host>/<owner>/<repo>/` | Plugin clones |
| `~/.cache/rvpm/merged/` | Linked root for `merge = true` plugins |
| `~/.cache/rvpm/loader.lua` | Generated loader |

Windows uses the same `.config` / `.cache` paths under `%USERPROFILE%` — no
`%APPDATA%` — to keep dotfiles portable between Linux/macOS/WSL/Windows.

`options.base_dir = "..."` moves all of `~/.cache/rvpm/` to a different root
(useful for dotfiles-managed caches). `options.loader_path = "..."` moves only
`loader.lua`.

## Development

```sh
# Build
cargo build

# Run the full test suite
cargo test

# Run a single test
cargo test test_loader_phase_order_init_rtp_before

# Format check / lint
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings

# Inspect the generated loader from the sample fixture
cargo test dump_full_sample_loader -- --ignored --nocapture
```

rvpm is developed with **TDD**: tests come first, and new behaviors are
covered by either unit or integration tests before implementation.

## Acknowledgments

- **[lazy.nvim](https://github.com/folke/lazy.nvim)** by `@folke` — the
  approach of taking over plugin loading entirely (`vim.go.loadplugins =
  false`), the `ftdetect` augroup wrapping trick, the `<Ignore>`-prefixed
  feedkeys replay, and the per-handler designs (`cmd.lua`, `keys.lua`,
  `event.lua`, `ft.lua`) were all studied and adapted for rvpm. rvpm is an
  independent Rust re-implementation inspired by these ideas.
- **[dvpm](https://github.com/yukimemi/dvpm)** — a Deno-based predecessor.

## License

MIT — see [LICENSE](LICENSE).
