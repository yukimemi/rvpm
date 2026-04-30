# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> Detailed references live under [`docs/`](docs/):
> - [`docs/architecture.md`](docs/architecture.md) — loader.lua phases, lazy triggers, lockfile / merge / path internals
> - [`docs/cli.md`](docs/cli.md) — full subcommand list & contributor checklist for adding flags

## Concept

- **Extremely Fast**: Blazing-fast startup via Rust concurrency (Tokio), a merged directory layout, and a pre-compiled loader.lua.
- **Type Safe & Robust**: TOML-based configuration typed with serde. The `resilience` principle ensures that one plugin's failure does not stop the whole system.
- **Convention over Configuration**: `init.lua` / `before.lua` / `after.lua` placed under `{config_root}/<host>/<owner>/<repo>/` are auto-loaded by convention.
- **Hybrid CLI**: One-shot operations via arguments alongside interactive operations through `FuzzySelect` / TUI.
- **Pre-compiled loader**: Disables Neovim's plugin loading with `vim.go.loadplugins = false` and emits a static loader.lua at generate time. Reduces startup I/O via merge optimization and pre-resolved globs.

## Git Workflow

- **Do not push directly to the main branch.** Always cut a feature branch and open a Pull Request.
- Exception: release-related chore commits like `chore: bump version to ...` or `chore: release vX.Y.Z`, and pushing `git tag vX.Y.Z`, may be pushed directly to main (existing history follows this pattern).
- Branch names should concisely describe the change (e.g. `feat/add-only-sync-new-plugin`).
- **Write PR titles and bodies in English.** Commit messages are also in English.

### PR Review Cycle

- Every PR runs reviews from **Gemini Code Assist** and **CodeRabbit**. Wait for both bots to post, address their comments (push fixes to the PR branch), and merge only after feedback is resolved.
- **Reply to reviewers after pushing a fix.** Reply on the corresponding review comment thread with an **@-mention (`@gemini-code-assist` / `@coderabbitai`)**. Silent fixes are invisible to reviewers, trigger blind re-reviews, and lose the audit trail (which fix addressed which comment).
- **After sending fix + reply, don't stop there — actively monitor for the bot's next response.** Every few minutes (about 5 minutes is a good cadence), poll `gh pr view` / `gh api .../pulls/<n>/comments` to check for bot replies. If a new actionable comment arrives, immediately fix → @-mention → resume monitoring. In an Agent environment, automate this with `/loop` or `ScheduleWakeup`.
- **Thread settle criteria**: A review thread is considered settled the moment **the latest bot reply is ack-only** ("Thank you" / "Understood" / "Acknowledged" / a re-review summary with no new findings, etc.). If the bot posts a `--diff` re-flag or another actionable comment, the thread reverts to unsettled.
- **Monitoring stop conditions**:
  1. **All open threads have settled** → the PR is quiet. When several PRs are being monitored concurrently (e.g. running fixes against two PRs in parallel), exit the polling loop and ask the owner for merge decisions only once **every** target PR has gone quiet — waiting on the slowest one. If the bot acks quickly, there is no need to wait 30 minutes.
  2. **30 minutes elapsed since the last actionable comment with no bot reply** → treat the thread as settled by timeout. This is a fallback for the case where the bot quietly gives up (stops emitting actionable comments and posts nothing). Too short (<10 min) misses delayed posts; too long (>1 hour) needlessly delays merges.
- **Merge gating.** Do not merge until **both** of the following are satisfied:
  1. Review bots (Gemini / CodeRabbit) stop emitting new actionable comments — keep the fix → @-mention → silence cycle running.
     Ack-only replies like "Understood" / "Thank you" from a bot count as the thread's quiet pass. If a new actionable comment arrives, restart the loop.
  2. The repository owner (@yukimemi) has explicitly approved the merge.
- **Exception: bot-authored PRs (Renovate, Dependabot).** Gemini and CodeRabbit skip these by default, so the "wait for bot review" gate does not apply. If CI is green and the owner approves, the PR may be merged.

## Development Commands

```bash
# Build
cargo build

# Run all tests
cargo test

# Run a single test (filter by module::function)
cargo test test_generate_loader_with_cond
cargo test loader::tests
cargo test git::tests::test_git_update_method_pulls_latest

# Release build
cargo build --release

# Visual debugging of loader.lua (ignored test)
cargo test dump_full_sample_loader -- --ignored --nocapture
```

## Design Principles

**Always implement using TDD.** Write tests first (and confirm they fail) before implementing.

**Resilience:** A single plugin's failure must not bring down the whole system. Sync failures and config mistakes (e.g. missing dependencies) are reported as warnings, and subsequent processing (`generate`, etc.) continues whenever possible. Safety at Neovim startup is the top priority — even an incomplete configuration must guarantee a minimal startup.

## TOML Configuration Schema

```toml
[vars]
# User-defined variables. Reference them from Tera templates inside the TOML as {{ vars.xxx }}.
repo_base   = "~/.cache/nvim/rvpm"
nvim_rc = "~/.config/nvim/rc"

[options]
# Root directory holding per-plugin init/before/after.lua.
# Defaults to ~/.config/rvpm/<appname>/plugins when unset.
config_root = "{{ vars.nvim_rc }}/plugins"
# Max parallelism (default 13, kept conservative to avoid GitHub rate limits).
concurrency = 16
# Auto-delete plugin directories that were dropped from config.toml on sync /
# generate completion (default false). Replaces having to pass `sync --prune` every time.
# auto_clean = true
# Auto-generate helptags via nvim --headless on sync / generate completion
# (default true). Lazy plugins are not on runtimepath, so rvpm enumerates the
# target doc/ directories itself and runs :helptags <path> for each.
# auto_helptags = false
# For lazy plugins with `merge = true`, also link their doc/ files into
# merged/doc/ so `:help <topic>` can find their tags before the plugin loads
# (default false). Only the doc/ subdirectory is linked — lua/ and plugin/
# stay out of merged/, so lazy semantics are preserved. Filename conflicts
# inside doc/ are first-wins.
# merge_lazy_doc = true
# URL form written by `rvpm add`: "short" (owner/repo, default) or
# "full" (https://github.com/owner/repo). Duplicate detection normalizes both forms before comparing.
# url_style = "full"
# Override rvpm's data root (defaults to ~/.cache/rvpm/<appname> when unset).
# repos / merged / loader.lua all live under `{cache_root}/plugins/`.
# cache_root = "~/.cache/nvim/rvpm"
# Post-scan auto-lazy suggestion policy for `rvpm add`:
#   "ask" (default) — TTY interactive prompt / skipped on non-TTY
#   "always"        — accept scan results unconditionally (for scripts)
#   "never"         — skip scanning, eager add
# auto_lazy = "ask"
# Backend used to delegate `rvpm add` to an AI CLI (#93).
#   "off" (default) — use the static scan + auto_lazy flow
#   "claude" / "gemini" / "codex" — spawn the corresponding CLI as a subprocess
# Errors out if the CLI is not installed. `auto_lazy` is ignored.
# CLI flags `--ai claude` / `--no-ai` allow per-call overrides.
# ai = "claude"
# Natural language used in AI output (explanation prose + chat replies). Default "en".
# The XML tag structure itself is fixed in English (for parse stability).
# ai_language = "ja"

[options.browse]
# Delegate README rendering to an external command (browse TUI only).
# Pipes raw markdown on stdin and converts ANSI escapes from stdout into
# ratatui Text via ansi-to-tui. Falls back to the built-in tui-markdown path on failure/timeout.
# Placeholders use Tera-style `{{ name }}` syntax (consistent with the rest of rvpm):
#   {{ width }} / {{ height }} / {{ file_path }} / {{ file_dir }}
#   {{ file_name }} / {{ file_stem }} / {{ file_ext }}
# readme_command = ["mdcat"]
# readme_command = ["glow", "-s", "dark", "-w", "{{ width }}", "{{ file_path }}"]

[[plugins]]
name  = "snacks"
url   = "folke/snacks.nvim"
# No on_* → eager (loaded at startup)

[[plugins]]
name = "telescope"
url  = "nvim-telescope/telescope.nvim"
depends = ["snacks.nvim"]
# rev: branch / tag / commit hash
# rev = "v0.1.0"
# build: shell command (run after sync / update completes, 5 min timeout)
# build = "cargo build --release"
# build_lua: Lua snippet executed via nvim --headless -u NONE -l (#97)
# Appends self + transitive depends to rtp; stdpath() reflects the real env, so
# native lib installs (e.g. blink.cmp) land properly in the user's data dir.
# build_lua = "require('blink.cmp').build():wait(60000)"

# Lazy-loading triggers (writing any one of these auto-infers lazy = true)
on_cmd    = ["Telescope", "/^Chezmoi/"]      # exact name or /regex/ (expanded by rvpm generate)
on_ft     = ["rust", "toml"]                 # string | string[]
on_event  = ["BufReadPre", "User LazyDone", "/^User Chezmoi/"]  # exact "User Xxx" or /regex/ also OK
on_path   = ["*.rs", "Cargo.toml"]           # BufRead/BufNewFile glob
on_source = ["snacks.nvim"]                  # triggered by another plugin's load-completion User event (specify by display_name)
# on_map allows mixing string (simple) and table (mode + desc) forms.
# Writing `/regex/` for lhs expands by matching against the plugin's <Plug>(...) list (#88).
on_map = [
  "<leader>f",                                              # mode = ["n"] (default)
  { lhs = "<leader>v",  mode = ["n", "x"] },
  { lhs = "<leader>g",  mode = ["n", "x"], desc = "Grep" },
  { lhs = "/^<Plug>\\(Chezmoi/", mode = ["n"] },           # bulk-lazy <Plug> family
]
# Conditional loading (Lua expression)
cond = "vim.fn.has('win32') == 1"
```

## Global hooks

Auto-applied just by placing files directly under `<config_root>/` (default `~/.config/rvpm/<appname>/`). No entries in the config file are needed (Convention over Configuration).

| File | Phase | Timing |
|---|---|---|
| `<config_root>/before.lua` | 3 | After the `load_lazy` helper is defined, before any plugin's `init.lua` |
| `<config_root>/after.lua` | 9 | After all lazy triggers are registered |

When `options.config_root` is unset, `<config_root>` is `~/.config/rvpm/<appname>` (`<appname>` = `$RVPM_APPNAME` → `$NVIM_APPNAME` → `nvim`).

`generate_loader()` takes a `LoaderOptions` struct (`global_before: Option<PathBuf>`, `global_after: Option<PathBuf>`) and embeds `dofile(...)` only when the file exists.

## per-plugin config files (config_root)

Per-plugin Lua config files can be placed under `options.config_root` using the `<host>/<owner>/<repo>/` hierarchy. Example: `~/.config/nvim/rc/plugins/github.com/nvim-telescope/telescope.nvim/`.

| File | Timing | Typical use |
|---|---|---|
| `init.lua` | **Before RTP append** (the pre-rtp phase, common to all plugins) | Pre-set variables like `vim.g.xxx_setting = ...` |
| `before.lua` | **Right after RTP append, before sourcing `plugin/*`** | Override setup, `require` lua/ modules, etc. |
| `after.lua` | **After sourcing `plugin/*`** | Post-setup that calls plugin functions, keymap configuration |

At generate time rvpm checks each file's existence and embeds `dofile(...)` in loader.lua only for ones that exist (pre-compiled).

## Architecture overview

`src/main.rs` is the entry point and command handler. Each command is implemented as a `run_*()` function and runs on the Tokio async runtime.

```
src/
  main.rs       — CLI definitions (clap), run_*() implementations for every command, helper functions
  config.rs     — TOML config parsing (with Tera template expansion), MapSpec type, sort_plugins
  doctor.rs     — `rvpm doctor` — 17 diagnostics × 4 categories + render (nerd/unicode/ascii)
  git.rs        — async wrappers for git clone/pull/fetch/checkout (Repo struct) + GitChange recording
  helptags.rs   — runs :helptags via nvim --headless to generate tags
  link.rs       — file-level linking into the merged directory (hard link, first-wins on conflict); `placed` returns newly placed files for winner tracking
  loader.rs     — logic that generates Neovim's loader.lua
  merge_conflicts.rs — read/write of `<cache_root>/merge_conflicts.json` (most recent sync only; consumed by doctor)
  lockfile.rs   — read/write of `<config_root>/rvpm.lock` (reproducible plugin versions; intended to be committed to dotfiles)
  tui.rs        — ratatui-based progress / list display TUI
  update_log.rs — read/append of `<cache_root>/update_log.json`, BREAKING detection, render
```

### Data flow

1. `parse_config()` — reads the TOML, expands Tera templates, then deserializes into the `Config` struct
2. `sort_plugins()` — topological sort based on the `depends` field (cycles produce only a warning)
3. `run_sync()` — parallel git clone/pull via `JoinSet` + `Semaphore` → link into the merged directory via `merge_plugin()` → pre-glob via `build_plugin_scripts()` → generate loader.lua via `generate_loader()` (which also runs the eager→lazy dependency promotion pre-pass) → `build_helptags()` launches `nvim --headless` to run `:helptags` (only when `options.auto_helptags=true`)

### loader.lua phase outline

```
Pre-pass:  eager→lazy dependency promotion
Phase 1:   vim.go.loadplugins = false
Phase 2:   define load_lazy helper
Phase 3:   global before.lua
Phase 4:   init.lua of every plugin (in dep order, pre-rtp)
Phase 5:   append merged/ to rtp once
Phase 6:   process eager plugins (rtp append + before/plugin/ftdetect/after-plugin/after.lua + User rvpm_loaded_<name>)
Phase 7:   register lazy plugin triggers (on_cmd / on_ft / on_map / on_event / on_path / on_source)
Phase 8:   register ColorSchemePre handlers (auto-detected for lazy plugins)
Phase 9:   global after.lua
```

See [`docs/architecture.md`](docs/architecture.md) for design rationale, lazy trigger details, and per-trigger mechanics.

### Key invariants

- **`vim.go.loadplugins = false`** is set in phase 1 — loader.lua is the single source of truth for plugin sourcing.
- **Pre-glob at generate time** — files under `plugin/`, `ftdetect/`, `after/plugin/` are walked once at generate time and embedded as literal paths in loader.lua. Zero glob calls at startup.
- **Lazy plugins stay off rtp** until their trigger fires — keeping `lua/` modules out of the rtp is what makes lazy meaningful.
- **Resilience** — failures during sync / link / helptags emit warnings on stderr and let subsequent steps continue.

### Lockfile priority order

`rev` in config > `commit` in lockfile > latest HEAD. `rvpm sync --frozen` errors when an entry is missing; `--no-lock` skips lockfile entirely. Details in [`docs/architecture.md`](docs/architecture.md).

### Merge strategy summary

`merge_plugin()` hard-links files into `<cache_root>/plugins/merged/` at file granularity (first-wins on conflict, dotfiles skipped, only rtp-relevant directories allow-listed). Lazy plugins are not merged (their `lua/` would leak onto rtp before the trigger fires). Conflicts are recorded in `merge_conflicts.json` and surfaced by `rvpm doctor`. Full rules in [`docs/architecture.md`](docs/architecture.md).

### Path conventions

Config / cache are **fixed at `~/.config/rvpm/` and `~/.cache/rvpm/` across all platforms** (no `%APPDATA%` on Windows — keeps dotfile layouts identical across WSL / Linux / Windows).

- `cache_root` (override: `options.cache_root`) → default `~/.cache/rvpm/<appname>` — moves repos / merged / loader.lua together.
- `config_root` (override: `options.config_root`) → default `~/.config/rvpm/<appname>/plugins` — per-plugin init/before/after.lua.
- `<appname>` resolves as `$RVPM_APPNAME` → `$NVIM_APPNAME` → `"nvim"`.

Always go through the `resolve_*` helpers in `src/main.rs` — never hardcode `.config/rvpm/...` or `.cache/rvpm/...` string literals. Full table of helpers and the directory layout in [`docs/architecture.md`](docs/architecture.md).

### Windows support

File-level hard links (`std::fs::hard_link`) on NTFS — no admin rights, no junctions, no symlinks. Falls back to `std::fs::copy` for cross-volume cases.

## CLI commands

`rvpm sync / generate / clean / add / tune / update / remove / edit / set / config / init / list / browse / doctor / profile / log`. Full flag-by-flag reference and the contributor checklist for adding flags is in [`docs/cli.md`](docs/cli.md).

## First-run support

`rvpm sync` / `rvpm generate` call `print_init_lua_hint_if_missing()` at the end and print guidance when Neovim's `init.lua` (resolved with `$NVIM_APPNAME`) does not reference loader.lua (or has not been created yet). Running `rvpm init --write` then either creates init.lua if absent or appends to its end (idempotently). The insertion is annotated so it is clearly identifiable as "added by rvpm."
