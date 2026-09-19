# CLI Reference

Detailed reference for every `rvpm` subcommand. The high-level overview lives
in [CLAUDE.md](../CLAUDE.md); this file is the authoritative source for flag
behavior and per-command edge cases.

## Command list

| Command | Function | Description |
|---------|------|------|
| `sync [--prune] [--frozen] [--no-lock] [--rebuild [QUERY]]` | `run_sync()` | clone/pull + merged + loader.lua generation. `--prune` also deletes unused plugin directories. Even without it, a warning is shown at the end if any are unused. Loads the lockfile (`<config_root>/rvpm.lock`) to align to pinned commits, and writes back the new HEAD on completion. `--frozen` errors immediately if any plugin is not registered (CI / fresh machine); `--no-lock` skips lockfile entirely. **Build runs only when git HEAD moved** (avoids re-running e.g. `:TSUpdate` on every no-op pull); `--rebuild` restores the previous always-build behavior. `--rebuild <QUERY>` narrows the rebuild scope to plugins whose url / name partially matches (for iterating on a single plugin's build command) — `matches_rebuild_filter` decides this, resolved into a bool before closure spawn so it does not get pulled into the async move. |
| `generate [--force]` | `run_generate()` | Regenerate loader.lua + views/merged. Unchanged plugins (same clone HEAD + merge mode, per `views/<plug>/.rvpm-stamp.json`) skip the hard-link rebuild, and helptags is skipped when nothing was touched — see "Incremental rebuild stamps" in [architecture.md](./architecture.md). `--force` ignores the stamps and rebuilds everything from scratch (use when build artifacts changed without a new commit). |
| `clean` | `run_clean()` | Delete `{cache_root}/plugins/repos/<host>/<owner>/<repo>/` for plugins removed from config.toml. No git ops, faster than `sync --prune` (matters on configs with 200+ plugins). Shares the helper `prune_unused_repos()` with `sync --prune`. |
| `add <repo> [--setup '{ … }'] [--auto-lazy \| --no-lazy]` | `run_add()` | TOML add + clone of just that plugin + generate. Duplicate detection normalizes via `installed_full_name` (absorbs https / owner/repo / ssh / case / `.git` / trailing `/` variation, sharing the same logic as the installed marker in `rvpm browse`). The written URL form follows `options.url_style` (`short` / `full`). **After clone, `plugin_scan::scan_plugin` runs to pick up user-facing commands / keymaps from the plugin's `plugin/` / `ftplugin/` / `after/plugin/` / `lua/`**, and based on `options.auto_lazy` (or `--auto-lazy` / `--no-lazy` overrides) chooses an interactive prompt / unconditional accept / skip (`AutoLazyPolicy::Ask/Always/Never`). On accept, `suggest_cmd_triggers_smart` LCP-clusters command groups (regex-izes them as `/^Prefix/` when there is a 3+ character common prefix); keymaps are enumerated; the corresponding `[[plugins]]` entry in `config.toml` gets `on_cmd` / `on_map` patched in place. The same scan also drives the **`setup = {}` suggestion**: when the plugin's main module resolves (`plugin_scan::resolve_main_module`, generate-time rules) and that module statically declares `setup` (`has_setup_function`), rvpm offers to write `setup = {}` into the entry so it calls `require("<module>").setup({})` for you. It shares the one prompt with the trigger suggestion and obeys the same `options.auto_lazy` policy (`ask` / `always` / `never`) plus the `--auto-lazy` / `--no-lazy` overrides; `never` / `--no-lazy` skip the scan (and therefore the `setup` suggestion) entirely. An explicitly eager entry (`lazy = false`) drops only the trigger half — the `setup` suggestion still shows, because an eager plugin needs its `setup()` just as much. Entries that already have `setup` are left alone. `--setup '{ … }'` writes the entry's `setup` field straight from the command line (TOML inline table only; `{}` / `{ notify = true }` / `{ main = "mini.pick", opts = {} }`), and marks the key as user-specified so the AI path and the scan suggestion leave it alone. |
| `tune [query] [--ai <backend>] [--no-ai]` | `run_tune()` | Run an AI chat loop (`run_ai_tune`) against **plugins already registered in the config**. Differences from `add --ai`: skips clone, shows the AI both the existing entry and existing hook bodies, and asks for "two variants — a fresh proposal (clean redesign) and a merged proposal (keep existing while improving)." On apply, the user picks per section: the `[[plugins]]` TOML entry uses `pick_plugin_entry_decision` (**fresh / merged / keep**, 3-way), and each per-plugin hook file (`init.lua` / `before.lua` / `after.lua`) uses `pick_hook_decision` which is the same 3-way **plus a `Remove existing` choice** when the user already has the file on disk (4-way; #115). `Replace` mode strips stale TOML fields the AI omitted (e.g. an outdated `on_cmd`). When the AI omits a hook tag entirely AND the user has the file, the hook menu collapses to a 2-choice `Keep / Remove existing` prompt with `[OMITTED BY AI]` flagged in the preview — `Remove` is hook-only and the menu defaults to `Keep` (safe-by-default), so deletion only happens on an explicit pick. User guardrails are exercised either by telling the AI "do not touch X" inside the chat loop or by selecting keep existing in preview. AI-only — if `effective_ai == Off`, errors explicitly (use `set` for non-AI tweaks). `tune` is also where an existing **setup-only `after.lua` gets folded into `setup`**: when the hook contains nothing but a `require('x').setup({ ...data... })` call, the AI is asked to move those values into the `[[plugins]]` entry's `setup` and propose `(none)` for the hook, which pairs with the `Remove existing` choice to delete the file. Bodies with Lua functions, `vim.*` calls, keymaps, or autocmds are never folded — TOML cannot hold them. |
| `update [query] [--no-cooldown]` | `run_update()` | Pull existing plugins (does not clone). On completion, overwrites the lockfile with the new HEAD (entries for non-target plugins are preserved even on partial update). The supply-chain cooldown is **on by default (1d)**: commits rvpm has observed for less than the window are **held back** (held plugins are listed in the summary with their tip age, and update advances to the newest matured observation instead when one exists). Set `options.cooldown = "0"` to disable, or `--no-cooldown` to bypass the gate for this run — use it to grab e.g. a security hotfix immediately. See "Supply-chain cooldown" in [architecture.md](./architecture.md). |
| `remove [query]` | `run_remove()` | TOML + directory deletion + generate. |
| `edit [query] [--init\|--before\|--after] [--global]` | `run_edit()` | Edit per-plugin init/before/after.lua in the editor. Flags skip file selection. `--global` edits global hooks — **`--init` directly opens Neovim's main `init.lua` (`nvim_init_lua_path()`)**, while `--before` / `--after` open `<config_root>/before.lua` / `after.lua`. This gives a consistent `init/before/after` 3-way UX between per-plugin and global. The `[ Global hooks ]` sentinel in interactive selection behaves the same. |
| `set [query] [flags]` | `run_set()` | Change lazy/merge/on_* etc. interactively or via arguments. `on_cmd` and friends accept comma-separated or JSON array; `--on-map` also supports JSON object/array for the table form. The `[ Open config.toml in $EDITOR ]` sentinel is an escape hatch for direct TOML editing. `set` deliberately does **not** touch `setup` (no interactive editor for nested tables): edit `setup` by hand, via `rvpm tune`, with `rvpm edit` / `rvpm config`, or set it at add time with `rvpm add --setup '{ … }'`. |
| `config` | `run_config()` | Open `config.toml` directly in `$EDITOR` (only `generate` runs on exit; if you added a new plugin, run `rvpm sync` explicitly). |
| `init [--write]` | `run_init()` | Show the `dofile(...)` snippet that wires loader.lua into Neovim's `init.lua`. `--write` appends it automatically (creates init.lua if absent). Honors `$NVIM_APPNAME`. |
| `list [--no-tui]` | `run_list()` | Plugin list display. Defaults to a TUI with action keys `[S] sync / [R] sync --rebuild / [u/U] update / [d] remove / [e] edit / [s] set / [t] tune / [c] config.toml / [b] browse / [?] help`. **The first row is the `[ Global hooks ]` sentinel** — `e` jumps to global edit (init/before/after); `u/d/s/t` are no-ops there. Navigation: `j/k/g/G/Ctrl-d/u/f/b`; search: `/n/N`. `--no-tui` outputs pipe-friendly plain text. |
| `browse` | `run_browse()` | Plugin browser TUI for the GitHub `neovim-plugin` topic (up to 300 entries, fetched in 3 pages). README is rendered as GFM via tui-markdown (set `options.browse.readme_command` to delegate to an external renderer like mdcat / glow, with a fallback). A leading `✓` marks installed entries; pressing `Enter` on an installed plugin warns and skips add. `/` is local incremental search (name + description + topics) with `n`/`N` for match jumps. `S` runs a GitHub API search. `Tab` toggles list/README focus. `o` opens the browser; `s` cycles sort; `R` clears cache and refetches; `c` opens config.toml in the editor; `l` jumps to the list TUI; `?` shows help. |
| `doctor` | `run_doctor()` | One-shot command that diagnoses 16 items across config / state / Neovim integration / external tools. 4 categories (plugin config / state integrity / Neovim integration / external tools); output respects `options.icons` (nerd/unicode/ascii). Exit codes: `0` = all ok, `1` = errors present, `2` = warnings only. External commands (nvim/git/chezmoi) are probed via `tokio::process::Command` + 2s timeout so they cannot hang. |
| `profile [--runs N (1..=20)] [--top N] [--json] [--no-tui] [--no-merge] [--no-instrument]` (`--json` and `--no-tui` cannot be combined) | `run_profile()` | Run `nvim --headless --startuptime` N times (default 3) and aggregate startup time per plugin. By default, temporarily swaps loader.lua for a **phase-instrumented build** (`LoaderSwapGuard` + atomic rename, restoring the original even on panic / Ctrl-C). Empty `.vim` markers for phase boundaries + per-plugin init/trig are placed in `tmp/rvpm-profile-markers-*/` ahead of time, and per-plugin times for phases 4/6/7 are extracted from the clock deltas of `vim.cmd("source <marker>")`. `--no-merge` passes `force_unmerge=true` to treat all plugins as merge=false (merged/ is left untouched; only the rtp append path changes). `--no-instrument` skips the swap and uses raw `--startuptime` only (same as v1). On startup, a stale `loader.lua.bak` from a prior crash is auto-restored (`recover_stale_loader_backup`). The TUI adds info via a phase timeline, init/load/trig columns, and a sort cycle (`s`). |
| `log [query] [--last N] [--full] [--diff]` | `run_log()` | Display the change history (`<cache_root>/update_log.json`) recorded during `sync` / `update` / `add`. `[query]` partially matches plugin names; `--last N` (default 1, max 20) shows the last N runs; `--diff` embeds README / CHANGELOG / doc/ patches; `--full` is reserved for future body display. Conventional Commits' `<type>!:` / `BREAKING CHANGE:` footers are highlighted with a `⚠ BREAKING` prefix. |
| `completion <SHELL>` | `run_completion()` | Print a shell completion script to stdout (#114). `SHELL` is one of `bash` / `zsh` / `fish` / `powershell` / `elvish` (clap_complete's supported set). Output is generated at runtime from the live `Cli` definition so new subcommands and flags are picked up automatically. Pipe into the appropriate location for the shell — see `rvpm completion --help` for example install paths. The Neovim-side completion lives separately in `rvpm.nvim` (`lua/rvpm/command.lua`) and is hand-maintained per the contributor checklist below. |

**Removed commands:**
- `status` → folded into `list --no-tui` (plain text output is feature-equivalent).

## TUI theme

Set `[options.theme]` in `config.toml` to customize the sync, update, list,
and browse screens. Omitted fields retain the existing colors; rvpm has no
built-in preset selector (no `theme_preset = "..."` key) — copy one of the
blocks below into `[options.theme]` instead. Values accept color names (such
as `"cyan"`, `"dark-gray"`, or `"reset"`), `"#RRGGBB"`, or integer palette
indices from 0 to 255. Invalid values warn and fall back independently;
unknown fields warn and are ignored.

### Copy-paste presets

Ported from each colorscheme's published palette; not pixel-perfect
matches to any specific Neovim plugin version.

**Catppuccin Mocha:**

```toml
[options.theme]
foreground = "#cdd6f4"
background = "#1e1e2e"
terminal_foreground = "#cdd6f4"
secondary = "#bac2de"
muted = "#6c7086"
success = "#a6e3a1"
warning = "#f9e2af"
error = "#f38ba8"
info = "#89dceb"
accent = "#cba6f7"
browse_accent = "#f9e2af"
selection_background = "#313244"
inverse = "#1e1e2e"
header_background = "#1e1e2e"
```

**Gruvbox Dark:**

```toml
[options.theme]
foreground = "#ebdbb2"
background = "#282828"
terminal_foreground = "#ebdbb2"
secondary = "#d5c4a1"
muted = "#928374"
success = "#b8bb26"
warning = "#fabd2f"
error = "#fb4934"
info = "#83a598"
accent = "#d3869b"
browse_accent = "#fabd2f"
selection_background = "#3c3836"
inverse = "#282828"
header_background = "#282828"
```

**Nord:**

```toml
[options.theme]
foreground = "#d8dee9"
background = "#2e3440"
terminal_foreground = "#d8dee9"
secondary = "#e5e9f0"
muted = "#4c566a"
success = "#a3be8c"
warning = "#ebcb8b"
error = "#bf616a"
info = "#88c0d0"
accent = "#b48ead"
browse_accent = "#ebcb8b"
selection_background = "#434c5e"
inverse = "#2e3440"
header_background = "#2e3440"
```

**Tokyo Night (Storm):**

```toml
[options.theme]
foreground = "#c0caf5"
background = "#1a1b26"
terminal_foreground = "#c0caf5"
secondary = "#a9b1d6"
muted = "#565f89"
success = "#9ece6a"
warning = "#e0af68"
error = "#f7768e"
info = "#7dcfff"
accent = "#bb9af7"
browse_accent = "#e0af68"
selection_background = "#283457"
inverse = "#1a1b26"
header_background = "#1a1b26"
```

**Dracula:**

```toml
[options.theme]
foreground = "#f8f8f2"
background = "#282a36"
terminal_foreground = "#f8f8f2"
secondary = "#f8f8f2"
muted = "#6272a4"
success = "#50fa7b"
warning = "#f1fa8c"
error = "#ff5555"
info = "#8be9fd"
accent = "#bd93f9"
browse_accent = "#ffb86c"
selection_background = "#44475a"
inverse = "#282a36"
header_background = "#282a36"
```

### Field reference

`foreground` colors primary text; `terminal_foreground` colors otherwise
unstyled text (default `"reset"`). `secondary` and `muted` color secondary text
and dimmed labels/borders. Status colors are `success`, `warning`, `error`,
and `info`; `accent` colors revision/dev labels, while `browse_accent` colors
browse controls. `inverse` is the foreground on colored badges.
`background`, `selection_background`, and `header_background` control the
screen, selected rows, and list header respectively.

This does not change the profile TUI, dialoguer prompts, plain-text output,
or README renderer syntax colors. In browse, editing the config with `c`
reloads the theme on return.

## Checklist when adding CLI flags / subcommands

When you **add, rename, or remove** a subcommand flag (`--prune` / `--ai` / `--no-tui` etc.) or **add a new subcommand**, also keep `lua/rvpm/command.lua` in [rvpm.nvim](https://github.com/yukimemi/rvpm.nvim) in sync. Specifically:

- New subcommand: add it to the `SUBCOMMANDS` array. If it should be routed to the TUI, register it in the `TUI` table; if it takes a plugin-name argument, register it in the `PLUGIN_ARG_SUBS` table; if it has flags, add an entry to the `FLAGS` table. Consider adding a convenience Lua API in `lua/rvpm/init.lua` as well.
- Adding/renaming/removing a flag on an existing subcommand: update the relevant `FLAGS[<sub>]` entry.

The rvpm.nvim side **hardcodes a mirror** of rvpm core's flag list to power `:Rvpm <sub> --<Tab>` completion (parsing `--help` dynamically was rejected on Neovim startup-cost grounds). Forgetting to sync causes silent drift in Neovim where "an existing flag is missing from completion" or "a removed flag still appears as a candidate." Add this to your CLI-PR self-review checklist.
