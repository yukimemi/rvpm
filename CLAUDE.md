# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

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

## Architecture

### Overall structure

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

### loader.lua generation strategy (`src/loader.rs`)

rvpm performs **full control over plugin loading** + **merge optimization** + **pre-glob at generate time**. Structure of loader.lua:

```
Pre-pass:  eager→lazy dependency promotion    ← if an eager plugin depends on a lazy one,
                                               promote that dep to eager and warn on stderr
Phase 1:   vim.go.loadplugins = false          ← disable Neovim's auto-source
Phase 2:   define load_lazy helper             ← runtime loader for lazy plugins (with double-load guard)
Phase 3:   global before.lua                   ← <config_root>/before.lua (when present)
Phase 4:   init.lua of every plugin (in dep order) ← pre-rtp phase
Phase 5:   append merged/ to rtp once          ← if any merge=true plugin exists
Phase 6:   process eager plugins in dep order:
             non-merge: vim.opt.rtp:append(plugin.path)
             before.lua
             source plugin/**/*.{vim,lua} directly using pre-globbed file names
             source ftdetect/**/*.{vim,lua} inside augroup filetypedetect
             source after/plugin/**/*.{vim,lua}
             after.lua
             fire User autocmd "rvpm_loaded_<name>" (for on_source chaining)
Phase 7:   register lazy plugin triggers      ← on_cmd / on_ft / on_map / on_event / on_path / on_source
             lazy→lazy dependency: trigger callback pre-loads the dep via load_lazy
Phase 8:   register ColorSchemePre handlers   ← auto-registered for lazy plugins where
                                               colors/*.{vim,lua} were detected at generate time. No config needed.
Phase 9:   global after.lua                   ← <config_root>/after.lua (when present)
```

Key design points:

- Setting `vim.go.loadplugins = false` halts Neovim's default plugin loading, so loader.lua sources everything explicitly. This avoids double-sourcing.
- Files under a plugin (`plugin/`, `ftdetect/`, `after/plugin/`) are **walked from disk at generate time**, with file paths embedded directly into loader.lua. Zero glob calls at startup.
- `ftdetect/` must be sourced inside `augroup filetypedetect`; otherwise filetype detection misbehaves.
- After loading a plugin, the `load_lazy()` helper fires `vim.api.nvim_exec_autocmds("User", { pattern = "rvpm_loaded_<name>" })`. This is required for `on_source` chaining. It also embeds a double-load guard via `loaded["<name>"] = true`.
- The `depends` field affects not only load order but **whether a plugin is loaded at all**: if an eager plugin references a lazy dep, the generate-time pre-pass promotes the dep to eager (with a stderr warning). If a lazy plugin references a lazy dep, the generated trigger callback pre-loads the dep via `load_lazy`.
- The `cond` field is wrapped as a Lua expression in `if cond then ... end`. Works for both eager and lazy plugins.
- **Auto-detected colorschemes**: when `colors/*.{vim,lua}` exists in the clone path of a lazy plugin, `generate_loader()` scans for those file names at generate time and auto-emits a phase 8 `ColorSchemePre` autocmd handler. No config file edits required. Eager plugins are unaffected because `colors/` is already on the RTP.
- **Auto-registered denops plugins**: when `denops/<name>/main.{ts,js}` exists in the clone path of a lazy plugin, `generate_loader()` scans for those paths at generate time and passes `{ {"<name>", "<abs main>"}, ... }` as the trailing argument to the `load_lazy()` call. Inside `load_lazy`, `pcall(vim.fn["denops#plugin#load"], name, script)` is issued so that after the rtp append + plugin/* source the plugin is explicitly registered with the denops daemon (denops.vim's auto-discover only fires once at VimEnter and does not pick up plugins that arrive on rtp later via lazy loading, so explicit registration is required). When denops.vim itself is not yet loaded, `pcall` silently skips it. Eager plugins do not need this because the VimEnter-time denops discovery walks the entire rtp.

### Change history via update_log.json (`src/update_log.rs`)

After a git pull during `sync` / `update` / `add`, "plugins that changed" are
appended to `<cache_root>/update_log.json`. `rvpm log` reads it back and emits
a human-readable digest.

Schema:
- `UpdateLog { runs: Vec<RunRecord> }`
- `RunRecord { timestamp, command, changes: Vec<ChangeRecord> }`
- `ChangeRecord { name, url, from, to, subjects, breaking_subjects, doc_files_changed }`

Key design:
- History is capped at **at most 20 runs** (oldest dropped). It does not grow unbounded.
- Writes use tempfile + atomic rename for race resilience.
- A run with empty changes (pull happened but HEAD did not move) is recorded but
  omitted by `rvpm log` (to reduce display noise).
- **BREAKING detection** is performed by the pure function `is_breaking(subject, body) -> bool`:
  - subject in Conventional Commits form `<type>!:` / `<type>(<scope>)!:`
  - body / footer contains a `BREAKING CHANGE:` (case-insensitive) line
- **Doc-change detection** runs `git diff --name-only <from>..<to> -- README* CHANGELOG* doc/`
  as a subprocess and records the file name list. The patch itself is not stored;
  it is fetched on demand from `git diff` when `rvpm log --diff` runs (avoiding
  size explosion).
- HEAD retrieval / commit walk / BREAKING detection on the git side use gix
  inside `Repo::sync` / `Repo::update` in `src/git.rs`, and return
  `Option<GitChange>`. Recording failures (e.g. disk full) do not stop the main
  flow (resilience).

### Reproducibility via rvpm.lock (`src/lockfile.rs`)

Same idea as `lazy.nvim`'s `lazy-lock.json`. `<config_root>/rvpm.lock` records
per-plugin pinned commit hashes; committing it with the dotfiles lets other
machines / fresh clones reproduce the same commit set.

Schema (TOML):
```toml
version = 1

[[plugins]]
name = "snacks.nvim"
url = "folke/snacks.nvim"
commit = "abc123..."
```

Priority order: **`rev` in config > `commit` in lockfile > latest HEAD**. A
plugin with `rev = "v1.2.3"` in config.toml takes top priority as an explicit
pin, then the lockfile commit, and finally — if neither — the default branch
HEAD is pulled.

Per-command behavior:
- `rvpm sync`: load lockfile → choose rev for each plugin per the priority
  above → `gix_checkout` → upsert post-sync HEAD → call `retain_by_names` at
  the end to drop entries for plugins removed from config → atomic save.
- `rvpm sync --frozen`: before sync starts, verify that all non-dev plugins in
  the config exist in the lockfile. Even one missing entry triggers an
  immediate `anyhow::bail!` — for cases requiring strict reproducibility on CI
  / fresh clones.
- `rvpm sync --no-lock`: skip both load and save of the lockfile. An existing
  dotfile lockfile is left untouched (not modified).
- `rvpm update [query]`: does **not** use the lockfile for checkout (always
  pull latest) but overwrites the lockfile with the new HEAD after the pull.
  Even on partial update (with query), entries for non-target plugins are
  preserved.
- `rvpm add <repo>`: upserts and saves only the single newly added plugin into
  the lockfile.

Implementation notes:
- `Repo::sync()` returns `None` on no-op (HEAD did not move), so for lockfile
  recording we additionally call `Repo::head_commit()` to get the current HEAD
  (ensuring an entry is established for both fresh-clone and no-op cases).
- `LockFile::save` performs a stable sort by name → minimizes dotfile diffs.
- Malformed / missing files emit a warning on stderr and fall back to an empty
  LockFile (resilience).
- `dev = true` plugins are excluded from the lockfile (they are local
  works-in-progress, so pinning a commit hash is meaningless).
- When `options.chezmoi = true`, the lockfile — like config.toml / hooks —
  goes through `chezmoi::write_path` + `chezmoi::apply` to write to the source
  side first and then propagate to the target. Skipping this collides with
  chezmoi's "source is truth" principle and would revert the lockfile to its
  old contents on the next `chezmoi apply`.
- `chezmoi::write_path` / `chezmoi::apply` are implemented as **async + 2s
  timeout** (`tokio::process::Command` + `tokio::time::timeout`). Same idea as
  the external-command probes in `run_doctor`: prevent rvpm from hanging due
  to a broken PATH shim or an unresponsive subprocess. `write_path` wraps
  `is_chezmoi_available` plus the multiple ancestor `chezmoi source-path`
  calls under a **single 2s budget** (so that individual timeouts do not
  accumulate into something orders of magnitude larger). On timeout, a warning
  is emitted on stderr and the target-side path is returned (resilience).

### Automatic helptags generation (`src/helptags.rs`)

On `sync` / `generate` completion, launch `nvim --headless --clean -c "source <tmp.vim>" -c "qa!"` once and run `:helptags <path>` against every target `doc/`. Disable via `options.auto_helptags = false`.

Why not embed it in loader.lua: rvpm's concept is to **prioritize Neovim startup speed above all else**. Generating helptags incurs an nvim process startup cost, so it is performed up-front on the rvpm side (sync/generate) rather than at Neovim startup.

Rules used by `collect_helptag_targets` to enumerate target `doc/`:
- If `merged_dir/doc/` exists, add it first — docs of merge=true & !lazy plugins are aggregated in one place, so a single `:helptags` call processes all of them.
- **Lazy plugins must be added individually even when merge=true** — the condition at `main.rs:566-568` keeps lazy plugins out of merged/, so each plugin's own `doc/` must be processed.
- Eager plugins with merge=false are also added individually.
- `cond` is evaluated at Lua runtime and cannot be judged from Rust, so all plugins are candidates (= those visible in `rvpm list` = targets).

Working around command-line argument length: to avoid hitting Windows' `CreateProcess` limit (~8KB), instead of stringing `-c "helptags d1" -c "helptags d2" ...` together, the tool writes a Vim script (wrapped in `try/catch`) to a tempfile and sources it in one go via `-c "source <tmp>"`.

Resilience: if `nvim` is not on PATH, only a warning is emitted and rvpm continues. Even if the nvim process exits non-zero, Ok is returned. Duplicate-tag warnings from `:helptags` (E154 etc.) are passed through to stderr — they carry value as an improvement signal for users who explicitly opt into merge, so they are not suppressed.

### lazy trigger implementation

Implementation per trigger:

| Trigger | Notes |
|---|---|
| `on_cmd` | `bang = true`, `range = true`, `nargs = "*"`, `complete` callback. The callback restores `event.bang / smods / fargs / range / count` and dispatches via `vim.cmd(cmd_table)`. Fully supports `:Foo!`, `:%Foo`, `:5Foo`, `:tab Foo`. The `"/regex/"` notation regex-matches at `rvpm generate` time against command names defined by the plugin in `plugin/`/`ftplugin/`/`after/plugin/`/`lua/` (`src/plugin_scan.rs` statically scans for `vim.api.nvim_create_user_command("Foo", …)` / `command! Foo`). Expansion results flow through the same emit path as the exact-name list — zero runtime cost, completion is not broken (all stubs are pre-registered). Dynamically defined commands (e.g. names decided via `vim.fn.input()`) cannot be picked up; specify them as exact names alongside, or fall back to literal. |
| `on_ft` | After loading, re-fires via `exec_autocmds("FileType", { buffer = ev.buf })` → the freshly loaded plugin's `ftplugin/<ft>.vim` fires for the current buffer. |
| `on_event` | The `"User Xxx"` syntax expands into a User event + pattern. After loading, re-fires via `exec_autocmds(ev.event, { buffer, data })`. The `"/regex/"` notation regex-matches against User event names that the plugin statically fires (`nvim_exec_autocmds("User", { pattern = "Foo" })` etc.) and expands them (#88). The `/regex/` matches against the synthesized `"User <name>"` string, so write things like `/^User Chezmoi/`. Standard events (`BufRead` etc.) cannot be enumerated statically and only pass through literally. |
| `on_path` | `BufRead` / `BufNewFile` glob patterns. Same re-fire via `exec_autocmds(ev.event, ...)`. |
| `on_map` | `vim.keymap.set({modes}, lhs, ..., { desc })`. The MapSpec type supports `lhs + mode[] + desc`. Replay is made safe by prefixing `<Ignore>` and using feedkeys. Writing `"/regex/"` for lhs expands by matching against the plugin's `<Plug>(...)` list (#88). The `<Plug>` family is the plugin's officially exposed API, so naming tends to be consistent and regex-ization pays off (e.g. `/^<Plug>\(Chezmoi/`). The original spec's `mode` / `desc` is inherited by each expanded entry. Zero matches / invalid regex are dropped + warned (emitting them literally would break the stub keymap path). |
| `on_source` | Chains loading off another plugin's `rvpm_loaded_<name>` User autocmd. |

By design `on_map` does not carry an `rhs` in its spec. Reasons:

- The combination of replay + after.lua picks up "the keymap that the plugin or user ultimately sets" (inside load_lazy, after.lua runs first, then feedkeys).
- Statically analyzing a plugin's internal keymaps is impractical.
- Edge cases that need `rhs` (count / operator) are largely covered by `"m"` mode feedkeys.
- An `rhs` field can be added later in a backward-compatible way if needed.

That said, `mode` is essential: if the mode in which rvpm installs its stub keymap does not match the mode of the keymap the user/plugin ultimately sets, the trigger never fires. The default is `["n"]`.

### Parallel execution and Semaphore

`run_sync()` and `run_update()` spawn parallel tasks via `tokio::task::JoinSet`. When `config.options.concurrency` is set, task count is bounded by `tokio::sync::Semaphore`.

```rust
let concurrency = resolve_concurrency(config.options.concurrency);
let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
// At the top of each task:
let _permit = sem.acquire_owned().await.unwrap();
```

### TOML config templating

`parse_config()` parses in two passes: first extract the vars section only → register `vars`, `env`, `is_windows` into a Tera context → render the entire TOML string → final parse. This makes `{{ vars.base }}` and `{{ env.HOME }}` usable inside the config file.

### Flexible schemas (`string | string[]` / `MapSpec` / etc)

`deserialize_string_or_vec` and `deserialize_map_specs` in `config.rs` use `serde(untagged)` enums to accept multiple TOML shapes.

- Both `on_cmd = "Foo"` and `on_cmd = ["Foo", "Bar"]` are OK.
- Both `on_map = ["<leader>f"]` and `on_map = [{ lhs = "...", mode = ["n", "x"] }]` are OK.

The write side (`set_plugin_list_field`) writes back as a string for one element and as an array for multiple (the minimal representation).

### merge strategy (`src/link.rs`)

`merge_plugin()` links into the merged directory **at file granularity**. Design highlights:

- **Files are hard-linked** (no admin rights required on Windows; stable on Unix). Same volume is required, but since repos / merged are both under `<cache_root>` this is fine. If hard-link fails (e.g. cross-volume), fall back to `std::fs::copy`. Junctions are directory-only and cannot be used for files. Symbolic links require admin rights on Windows and are therefore not used.
- **Directories are just created** (`create_dir_all`). The directory itself is a real directory; its contents are recursively linked file by file. The previous junction-per-directory scheme would, when multiple plugins place files under the same hierarchy (e.g. several cmp plugins sharing `lua/cmp/`), cause last-writer-wins overwrites and clobber earlier contents.
- **First-wins + conflict summary** — on conflict, the new file is skipped and a `MergeConflict { relative }` is collected. `MergeResult.placed` returns the list of files newly placed in this run, and main.rs maintains a `HashMap<PathBuf, String>` to **look up the winner plugin name** (loser-only would not tell you "which plugin did it collide with?"). At the end of `run_sync` / `run_generate`, `print_merge_conflicts` groups results by plugin, displays each line on stderr with `(kept: <winner>)` appended, and overwrites `<cache_root>/merge_conflicts.json` each time. `rvpm doctor` reads the latter and surfaces it as a warning.
- **Files at the plugin root are ignored** — README.md / LICENSE / Makefile / package.json / *.toml and other meta files have no place on the rtp; they would only become noise that collides across plugins.
- **Directories at the plugin root are allow-listed to rtp conventions + denops** — `plugin/`, `lua/`, `doc/`, `ftplugin/`, `ftdetect/`, `syntax/`, `indent/`, `colors/`, `compiler/`, `autoload/`, `after/`, `queries/`, `parser/`, `rplugin/`, `spell/`, `keymap/`, `lang/`, `pack/`, `tutor/` (for `:Tutor`), and `denops/` (for denops.vim TypeScript plugins). `tests/` `scripts/` `examples/` `src/` etc. are unrelated to the rtp and are excluded.
- **Skip dotfiles at every level** (`.gitignore`, `.luarc.json`, `.editorconfig`, `.gitkeep`, etc.) — they are unrelated to Neovim startup, and at deep levels (e.g. `doc/.gitignore`) would just collide across plugins and add conflict-warning noise.

### Windows support

Once the merge strategy switched to file-level hard links, the setup no longer requires admin rights on Windows and uses neither junctions nor symbolic links. `std::fs::hard_link` works on NTFS without admin. Directories are created with `create_dir_all`, so junctions are not needed. The symbolic-link permission issue is avoided.

### Path conventions (fixed + overridable)

Config / cache are **fixed at `~/.config/rvpm/` and `~/.cache/rvpm/` across all platforms**. Even on Windows, `dirs::config_dir()` (`%APPDATA%`) is not used. Reasons:

- Aligns with Neovim's convention (`~/.config/nvim`).
- Lets dotfiles share an identical path layout across WSL / Linux / Windows.
- A single mental model is enough.

#### Path helpers (src/main.rs)

| Helper | Purpose | Override |
|---|---|---|
| `rvpm_config_path()` | `~/.config/rvpm/config.toml` | **Fixed** (avoids chicken-and-egg) |
| `resolve_cache_root(opt)` | `~/.cache/rvpm/<appname>` or tilde-expanded `opt` | `options.cache_root` |
| `resolve_repos_dir(cache_root)` | `{cache_root}/plugins/repos` | — |
| `resolve_merged_dir(cache_root)` | `{cache_root}/plugins/merged` | — |
| `resolve_loader_path(cache_root)` | `{cache_root}/plugins/loader.lua` | — |
| `resolve_config_root(opt)` | `~/.config/rvpm/<appname>/plugins/` or `opt` | `options.config_root` |
| `expand_tilde(s)` | General-purpose helper that expands `~` / `~/...` / `~\...` to home dir | — |

Do not write `.config/rvpm/...` or `.cache/rvpm/...` as string literals in code. Always go through a helper.

#### Resolution order

- **cache_root**: `options.cache_root` (tilde-expanded) → default `~/.cache/rvpm/<appname>`
- **config_root**: `options.config_root` (tilde-expanded) → `~/.config/rvpm/<appname>/plugins`
- **repos**: always `{cache_root}/plugins/repos/<canonical>/` (per-plugin override is `plugin.dst`)
- **merged**: always `{cache_root}/plugins/merged/`
- **loader**: always `{cache_root}/plugins/loader.lua`

In other words, setting just `options.cache_root` moves repos / merged / loader.lua together. `options.config_root` overrides only the per-plugin init/before/after.lua location, and defaults to `~/.config/rvpm/<appname>/plugins/` next to config.toml.

### CLI command list

| Command | Function | Description |
|---------|------|------|
| `sync [--prune] [--frozen] [--no-lock] [--rebuild [QUERY]]` | `run_sync()` | clone/pull + merged + loader.lua generation. `--prune` also deletes unused plugin directories. Even without it, a warning is shown at the end if any are unused. Loads the lockfile (`<config_root>/rvpm.lock`) to align to pinned commits, and writes back the new HEAD on completion. `--frozen` errors immediately if any plugin is not registered (CI / fresh machine); `--no-lock` skips lockfile entirely. **Build runs only when git HEAD moved** (avoids re-running e.g. `:TSUpdate` on every no-op pull); `--rebuild` restores the previous always-build behavior. `--rebuild <QUERY>` narrows the rebuild scope to plugins whose url / name partially matches (for iterating on a single plugin's build command) — `matches_rebuild_filter` decides this, resolved into a bool before closure spawn so it does not get pulled into the async move. |
| `generate` | `run_generate()` | Regenerate loader.lua only. |
| `clean` | `run_clean()` | Delete `{cache_root}/plugins/repos/<host>/<owner>/<repo>/` for plugins removed from config.toml. No git ops, faster than `sync --prune` (matters on configs with 200+ plugins). Shares the helper `prune_unused_repos()` with `sync --prune`. |
| `add <repo> [--auto-lazy \| --no-lazy]` | `run_add()` | TOML add + clone of just that plugin + generate. Duplicate detection normalizes via `installed_full_name` (absorbs https / owner/repo / ssh / case / `.git` / trailing `/` variation, sharing the same logic as the installed marker in `rvpm browse`). The written URL form follows `options.url_style` (`short` / `full`). **After clone, `plugin_scan::scan_plugin` runs to pick up user-facing commands / keymaps from the plugin's `plugin/` / `ftplugin/` / `after/plugin/` / `lua/`**, and based on `options.auto_lazy` (or `--auto-lazy` / `--no-lazy` overrides) chooses an interactive prompt / unconditional accept / skip (`AutoLazyPolicy::Ask/Always/Never`). On accept, `suggest_cmd_triggers_smart` LCP-clusters command groups (regex-izes them as `/^Prefix/` when there is a 3+ character common prefix); keymaps are enumerated; the corresponding `[[plugins]]` entry in `config.toml` gets `on_cmd` / `on_map` patched in place. |
| `tune [query] [--ai <backend>] [--no-ai]` | `run_tune()` | Run an AI chat loop (`run_ai_tune`) against **plugins already registered in the config**. Differences from `add --ai`: skips clone, shows the AI both the existing entry and existing hook bodies, and asks for "two variants — a fresh proposal (clean redesign) and a merged proposal (keep existing while improving)." On apply, the user picks **fresh / merged / keep existing** per section (`pick_plugin_entry_decision` / `pick_hook_decision`). `Replace` mode removes stale fields the AI omitted (e.g. an outdated `on_cmd`). User guardrails are exercised either by telling the AI "do not touch X" inside the chat loop or by selecting keep existing in preview. AI-only — if `effective_ai == Off`, errors explicitly (use `set` for non-AI tweaks). |
| `update [query]` | `run_update()` | Pull existing plugins (does not clone). On completion, overwrites the lockfile with the new HEAD (entries for non-target plugins are preserved even on partial update). |
| `remove [query]` | `run_remove()` | TOML + directory deletion + generate. |
| `edit [query] [--init\|--before\|--after] [--global]` | `run_edit()` | Edit per-plugin init/before/after.lua in the editor. Flags skip file selection. `--global` edits global hooks — **`--init` directly opens Neovim's main `init.lua` (`nvim_init_lua_path()`)**, while `--before` / `--after` open `<config_root>/before.lua` / `after.lua`. This gives a consistent `init/before/after` 3-way UX between per-plugin and global. The `[ Global hooks ]` sentinel in interactive selection behaves the same. |
| `set [query] [flags]` | `run_set()` | Change lazy/merge/on_* etc. interactively or via arguments. `on_cmd` and friends accept comma-separated or JSON array; `--on-map` also supports JSON object/array for the table form. The `[ Open config.toml in $EDITOR ]` sentinel is an escape hatch for direct TOML editing. |
| `config` | `run_config()` | Open `config.toml` directly in `$EDITOR` (only `generate` runs on exit; if you added a new plugin, run `rvpm sync` explicitly). |
| `init [--write]` | `run_init()` | Show the `dofile(...)` snippet that wires loader.lua into Neovim's `init.lua`. `--write` appends it automatically (creates init.lua if absent). Honors `$NVIM_APPNAME`. |
| `list [--no-tui]` | `run_list()` | Plugin list display. Defaults to a TUI with action keys `[S] sync / [R] sync --rebuild / [u/U] update / [d] remove / [e] edit / [s] set / [t] tune / [c] config.toml / [b] browse / [?] help`. **The first row is the `[ Global hooks ]` sentinel** — `e` jumps to global edit (init/before/after); `u/d/s/t` are no-ops there. Navigation: `j/k/g/G/Ctrl-d/u/f/b`; search: `/n/N`. `--no-tui` outputs pipe-friendly plain text. |
| `browse` | `run_browse()` | Plugin browser TUI for the GitHub `neovim-plugin` topic (up to 300 entries, fetched in 3 pages). README is rendered as GFM via tui-markdown (set `options.browse.readme_command` to delegate to an external renderer like mdcat / glow, with a fallback). A leading `✓` marks installed entries; pressing `Enter` on an installed plugin warns and skips add. `/` is local incremental search (name + description + topics) with `n`/`N` for match jumps. `S` runs a GitHub API search. `Tab` toggles list/README focus. `o` opens the browser; `s` cycles sort; `R` clears cache and refetches; `c` opens config.toml in the editor; `l` jumps to the list TUI; `?` shows help. |
| `doctor` | `run_doctor()` | One-shot command that diagnoses 16 items across config / state / Neovim integration / external tools. 4 categories (plugin config / state integrity / Neovim integration / external tools); output respects `options.icons` (nerd/unicode/ascii). Exit codes: `0` = all ok, `1` = errors present, `2` = warnings only. External commands (nvim/git/chezmoi) are probed via `tokio::process::Command` + 2s timeout so they cannot hang. |
| `profile [--runs N (1..=20)] [--top N] [--json] [--no-tui] [--no-merge] [--no-instrument]` (`--json` and `--no-tui` cannot be combined) | `run_profile()` | Run `nvim --headless --startuptime` N times (default 3) and aggregate startup time per plugin. By default, temporarily swaps loader.lua for a **phase-instrumented build** (`LoaderSwapGuard` + atomic rename, restoring the original even on panic / Ctrl-C). Empty `.vim` markers for phase boundaries + per-plugin init/trig are placed in `tmp/rvpm-profile-markers-*/` ahead of time, and per-plugin times for phases 4/6/7 are extracted from the clock deltas of `vim.cmd("source <marker>")`. `--no-merge` passes `force_unmerge=true` to treat all plugins as merge=false (merged/ is left untouched; only the rtp append path changes). `--no-instrument` skips the swap and uses raw `--startuptime` only (same as v1). On startup, a stale `loader.lua.bak` from a prior crash is auto-restored (`recover_stale_loader_backup`). The TUI adds info via a phase timeline, init/load/trig columns, and a sort cycle (`s`). |
| `log [query] [--last N] [--full] [--diff]` | `run_log()` | Display the change history (`<cache_root>/update_log.json`) recorded during `sync` / `update` / `add`. `[query]` partially matches plugin names; `--last N` (default 1, max 20) shows the last N runs; `--diff` embeds README / CHANGELOG / doc/ patches; `--full` is reserved for future body display. Conventional Commits' `<type>!:` / `BREAKING CHANGE:` footers are highlighted with a `⚠ BREAKING` prefix. |

**Removed commands:**
- `status` → folded into `list --no-tui` (plain text output is feature-equivalent).

### Checklist when adding CLI flags / subcommands

When you **add, rename, or remove** a subcommand flag (`--prune` / `--ai` / `--no-tui` etc.) or **add a new subcommand**, also keep `lua/rvpm/command.lua` in [rvpm.nvim](https://github.com/yukimemi/rvpm.nvim) in sync. Specifically:

- New subcommand: add it to the `SUBCOMMANDS` array. If it should be routed to the TUI, register it in the `TUI` table; if it takes a plugin-name argument, register it in the `PLUGIN_ARG_SUBS` table; if it has flags, add an entry to the `FLAGS` table. Consider adding a convenience Lua API in `lua/rvpm/init.lua` as well.
- Adding/renaming/removing a flag on an existing subcommand: update the relevant `FLAGS[<sub>]` entry.

The rvpm.nvim side **hardcodes a mirror** of rvpm core's flag list to power `:Rvpm <sub> --<Tab>` completion (parsing `--help` dynamically was rejected on Neovim startup-cost grounds). Forgetting to sync causes silent drift in Neovim where "an existing flag is missing from completion" or "a removed flag still appears as a candidate." Add this to your CLI-PR self-review checklist.

### Directory layout (default)

| Path | Purpose |
|------|------|
| `~/.config/rvpm/config.toml` | Main configuration file (**fixed regardless of appname** — to avoid chicken-and-egg) |
| `~/.config/rvpm/<appname>/before.lua` | Global before hook (phase 3, before all init.lua; auto-applied if present) |
| `~/.config/rvpm/<appname>/after.lua` | Global after hook (phase 9, after all lazy triggers are registered; auto-applied if present) |
| `~/.config/rvpm/<appname>/plugins/<host>/<owner>/<repo>/` | Per-plugin init/before/after.lua (override via `options.config_root`) |
| `~/.config/rvpm/<appname>/rvpm.lock` | Lockfile of plugin commit pins (override via `options.config_root`). Commit it with your dotfiles to reproduce on other machines. |
| `~/.cache/rvpm/<appname>/plugins/repos/<host>/<owner>/<repo>/` | Plugin clone destination |
| `~/.cache/rvpm/<appname>/plugins/merged/` | Aggregated link target for merge=true plugins |
| `~/.cache/rvpm/<appname>/plugins/loader.lua` | Generated Neovim loader |
| `~/.cache/rvpm/<appname>/plugins/merged/doc/tags` | Aggregated tags for merge=true plugins (generated by `:helptags`) |
| `~/.cache/rvpm/<appname>/plugins/repos/<host>/<owner>/<repo>/doc/tags` | Per-plugin tags for lazy / merge=false plugins |
| `~/.cache/rvpm/<appname>/update_log.json` | Change history of `sync` / `update` / `add` runs (read by `rvpm log`, max 20 runs) |
| `~/.cache/rvpm/<appname>/merge_conflicts.json` | Snapshot of merge conflicts from the latest `sync` / `generate` (read by `rvpm doctor`). Not history — overwritten each run. |

`<appname>` is determined as `$RVPM_APPNAME` → `$NVIM_APPNAME` → `"nvim"`, in that order. Setting `options.cache_root` moves the entire `~/.cache/rvpm/<appname>/` (repos/merged/loader.lua). `options.config_root` independently moves the per-plugin config directory.

### First-run support

`rvpm sync` / `rvpm generate` call `print_init_lua_hint_if_missing()` at the end and print guidance when Neovim's `init.lua` (resolved with `$NVIM_APPNAME`) does not reference loader.lua (or has not been created yet). Running `rvpm init --write` then either creates init.lua if absent or appends to its end (idempotently). The insertion is annotated so it is clearly identifiable as "added by rvpm."
