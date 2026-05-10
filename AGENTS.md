# AGENTS.md

Guidance for AI agents (Claude / Codex / Gemini) working in this
repo. The yukimemi/* shared conventions live in the
`<!-- kata:agents:* -->` blocks below, sourced from
`yukimemi/pj-base` / `pj-rust` / `pj-rust-cli` via `kata apply` —
see those for git workflow, PR review cycle, build/lint/test
commands, release flow, and renri's worktree usage.

The sections above the marker blocks are rvpm-specific and
consumer-owned: edit them freely; `kata apply` won't touch them.

> Detailed references live under [`docs/`](docs/):
> - [`docs/architecture.md`](docs/architecture.md) — loader.lua phases, lazy triggers, lockfile / merge / path internals
> - [`docs/cli.md`](docs/cli.md) — full subcommand list & contributor checklist for adding flags

## Concept

- **Extremely Fast**: Blazing-fast startup via Rust concurrency (Tokio), a merged directory layout, and a pre-compiled loader.lua.
- **Type Safe & Robust**: TOML-based configuration typed with serde. The `resilience` principle ensures that one plugin's failure does not stop the whole system.
- **Convention over Configuration**: `init.lua` / `before.lua` / `after.lua` placed under `{config_root}/<host>/<owner>/<repo>/` are auto-loaded by convention.
- **Hybrid CLI**: One-shot operations via arguments alongside interactive operations through `FuzzySelect` / TUI.
- **Pre-compiled loader**: Disables Neovim's plugin loading with `vim.go.loadplugins = false` and emits a static loader.lua at generate time. Reduces startup I/O via merge optimization and pre-resolved globs.

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
# Aggregate every non-Full plugin's `doc/` into `merged/doc/` so `:help <topic>`
# can find their tags before the plugin loads (default false). The plugin's
# rtp entry routes through `views/<plug>/` (a doc-stripped, hard-link tree of
# the clone) so `:tselect` shows no duplicate. Eager + merge=true (Full merge)
# is unaffected — those still go entirely into `merged/`. Per-plugin override
# is `[[plugins]] merge_doc = true|false`. Filename conflicts inside `doc/`
# are first-wins (recorded in `merge_conflicts.json`). #119
# merge_doc = true
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
#   "claude" / "gemini" / "codex" / "opencode" — spawn the corresponding CLI as a subprocess
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
# rev: branch / tag / commit hash, or `/regex/` to pick the highest semver tag matching the pattern
# rev = "v0.1.0"
# rev = "/^v1\\..*/"   # picks max semver tag among /^v1\..*/ — re-resolves on every sync
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
# Per-plugin override of `options.merge_doc` (#119).
# - Some(true)  → for this plugin, merge `doc/` into `merged/doc/` and route rtp through `views/<plug>/`
# - Some(false) → opt out of doc-merge even when global default is true
# - omitted     → follow `options.merge_doc`. With `cond` set, an unset value is auto-forced false
#                 (only sweeps cond plugins out of the global default — explicit Some(true) is honored)
# merge_doc = true
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

```text
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

```text
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

`decide_merge_mode(plugin.merge, plugin.lazy, plugin.merge_doc, options.merge_doc)` chooses one of three actions per plugin (#119):

- **Full** (`merge=true && eager`) → all rtp dirs hard-linked into `<cache_root>/plugins/merged/`; rtp gets `merged/` once at startup.
- **ViewWithDoc** (everything else, `merge_doc=false`) → all rtp dirs (incl. `doc/`) hard-linked into `<cache_root>/plugins/views/<plug>/`; rtp gets that view path (eager: startup; lazy: at trigger).
- **ViewWithoutDoc** (everything else, `merge_doc=true`) → view tree minus `doc/`, plus the plugin's `doc/` files aggregated into `merged/doc/`. rtp gets the doc-stripped view; `:help` works through `merged/` from startup, no `:tselect` duplicate after trigger.

`repos/<plug>/` is **never on rtp** — only `merged/` and `views/<plug>/` are. Conflicts are first-wins, recorded in `merge_conflicts.json` (self-conflicts filtered), surfaced by `rvpm doctor`. `cond` plugins get `merge=false` forced by `disable_merge_if_cond`, and `merge_doc=None` is forced to `Some(false)` (explicit per-plugin `Some(true)` survives — Windows-only-but-help-findable use case). Full rules in [`docs/architecture.md`](docs/architecture.md).

### Path conventions

Config / cache are **fixed at `~/.config/rvpm/` and `~/.cache/rvpm/` across all platforms** (no `%APPDATA%` on Windows — keeps dotfile layouts identical across WSL / Linux / Windows).

- `cache_root` (override: `options.cache_root`) → default `~/.cache/rvpm/<appname>` — moves repos / merged / views / loader.lua together.
- `config_root` (override: `options.config_root`) → default `~/.config/rvpm/<appname>/plugins` — per-plugin init/before/after.lua.
- `<appname>` resolves as `$RVPM_APPNAME` → `$NVIM_APPNAME` → `"nvim"`.

Always go through the `resolve_*` helpers in `src/main.rs` — never hardcode `.config/rvpm/...` or `.cache/rvpm/...` string literals. Full table of helpers and the directory layout in [`docs/architecture.md`](docs/architecture.md).

### Windows support

Plugin contents are merged with file-level hard links (`std::fs::hard_link`) on NTFS — no admin rights required. Falls back to `std::fs::copy` for cross-volume cases.

The single exception is the per-view `.git` indirection: `views/<plug>/.git` is a directory junction (Windows; created via the `junction` crate, no `mklink` cmd-spawn) or symlink (Unix) pointing to the plugin's clone `.git` dir. Junctions also need no admin rights. This is required for plugins that detect their own git state from the rtp dir (e.g. blink.cmp's `vim.fs.root('.git')` + `git describe --tags`).

## CLI commands

`rvpm sync / generate / clean / add / tune / update / remove / edit / set / config / init / list / browse / doctor / profile / log / completion`. Full flag-by-flag reference and the contributor checklist for adding flags is in [`docs/cli.md`](docs/cli.md).

## First-run support

`rvpm sync` / `rvpm generate` call `print_init_lua_hint_if_missing()` at the end and print guidance when Neovim's `init.lua` (resolved with `$NVIM_APPNAME`) does not reference loader.lua (or has not been created yet). Running `rvpm init --write` then either creates init.lua if absent or appends to its end (idempotently). The insertion is annotated so it is clearly identifiable as "added by rvpm."

<!-- kata:agents:base:begin -->
## yukimemi/* shared conventions

This file is the agent-agnostic source of truth (per the
[agents.md](https://agents.md) convention). The matching
`CLAUDE.md` and `GEMINI.md` files are thin shims that point back
here so each tool's auto-load behaviour still finds something.
**Edit AGENTS.md, not the shims.**

### Git workflow

- **No direct push to `main`.** Open a PR.
  - Exception: trivial typo / whitespace / docs wording fixes.
  - Exception: standalone version bumps.
- Branch names: `feat/...`, `fix/...`, `chore/...`.
- **PR titles + bodies in English. Commit messages in English.**
- Tag-based releases: `git tag vX.Y.Z && git push origin vX.Y.Z`.

### PR review cycle

- Every PR runs reviews from **Gemini Code Assist** and
  **CodeRabbit**. Wait for both bots to post, address their
  comments (push fixes to the PR branch), and merge only after
  feedback is resolved.
- **Reply to reviewers after pushing a fix.** Reply on the
  corresponding review thread with an **@-mention**
  (`@gemini-code-assist` / `@coderabbitai`). Silent fixes are
  invisible to reviewers and cost the audit trail.
- A review thread is **settled** the moment the latest bot reply
  is ack-only ("Thank you" / "Understood" / a re-review summary
  with no new findings) or 30 minutes elapse with no actionable
  comment.
- **Merge gate**: review bots quiet AND owner explicit approval.
- Bot-authored PRs (Renovate / Dependabot) skip the bot-review
  gate; CI green + owner approval is enough.

### Worktree workflow

Use [`renri`](https://github.com/yukimemi/renri) for any
commit-bound change. From the main checkout:

```sh
renri add <branch-name>            # create a worktree (jj-first)
renri --vcs git add <branch-name>  # force a git worktree
renri remove <branch-name>         # cleanup after merge
renri prune                        # GC stale worktrees
```

Read-only inspection can stay on the main checkout.

### kata-managed sections

Several files in this repo are managed by `kata apply` from the
[`yukimemi/pj-presets`](https://github.com/yukimemi/pj-presets)
templates — the bytes between `<!-- kata:*:begin -->` and
`<!-- kata:*:end -->` markers, plus the overwrite-always files
listed in `.kata/applied.toml`. **Editing those bytes locally
won't survive the next `kata apply`** — push the change to the
upstream template repo (`yukimemi/pj-base` / `yukimemi/pj-rust` /
…) instead. The marker scopes are layered:

- `kata:agents:base:*` — language-agnostic conventions (this section).
- `kata:agents:rust:*` — added when `pj-rust` applies.
- `kata:agents:rust-cli:*` — added when `pj-rust-cli` applies.
<!-- kata:agents:base:end -->
<!-- kata:agents:rust:begin -->
### Rust workflow

This repo follows the yukimemi/* Rust toolchain conventions. The
language-agnostic conventions block above (`kata:agents:base:*`)
covers git workflow, PR review cycle, and worktree usage.

### Build / lint / test

```sh
cargo make check                    # fmt --check + clippy + test + lock-check (the pre-push gate)
cargo make setup                    # one-time hook install + apm install
cargo build                         # debug build
cargo build --release               # release build
cargo test                          # tests; add -- --nocapture for stdout
```

`cargo make check` is what `.github/workflows/ci.yml` runs and what
the local pre-push hook calls — anything that passes locally
should pass on CI and vice versa. Don't paper over a failing
clippy by sprinkling `#[allow(clippy::...)]`; fix the underlying
issue or push back on the lint with reasoning.

### Toolchain pin

The Rust toolchain is pinned via `rust-toolchain.toml` and the
project compiles with the `stable` channel. Don't introduce
nightly-only features without a real reason; if you do, document
the reason in the relevant module.

### Lint / format policy

`rustfmt.toml` and `clippy.toml` are kata-managed (sourced from
`yukimemi/pj-rust`). Edits to those files in this repo won't
survive the next `kata apply`; if a setting is wrong, push the
fix to `yukimemi/pj-rust` so every yukimemi/* Rust project picks
it up.

### CI workflow

`.github/workflows/ci.yml` is also kata-managed. The source lives
in `yukimemi/pj-rust/.github/workflows/ci.yml.template` (the
`.template` suffix keeps GitHub Actions from running the source
itself in pj-rust); each Rust project receives the rendered
`ci.yml` via `kata apply`. Action versions are bumped centrally
by Renovate at `yukimemi/pj-rust` and propagate down on the next
apply, so don't bump them locally — Renovate is configured
(via the kata-distributed `renovate.json`) to ignore
`.github/workflows/ci.yml` and `.github/workflows/release.yml`
in each PJ to avoid the bump→clobber loop.
<!-- kata:agents:rust:end -->
<!-- kata:agents:rust-cli:begin -->
### Rust CLI release flow

This is a Rust CLI crate, so the release pipeline is publish-aware.
`yukimemi/pj-rust-cli` ships a tag-driven release workflow in
`.github/workflows/release.yml` (rendered from
`release.yml.template` for the same don't-auto-execute reason
ci.yml uses).

```sh
# Bump `package.version` in Cargo.toml (run `cargo build` so
# Cargo.lock follows), then:
git commit -am "chore: bump version to X.Y.Z"
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin main vX.Y.Z
```

The workflow then:
1. Cross-compiles binaries for x86_64 Linux / Windows / macOS,
   plus aarch64 macOS (Apple Silicon) — full triples
   `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
   `x86_64-apple-darwin`, `aarch64-apple-darwin`.
2. Uploads them as a GitHub Release with auto-generated notes.
3. `cargo publish --locked` to crates.io using the
   `CARGO_REGISTRY_TOKEN` repo secret.

Set the `CARGO_REGISTRY_TOKEN` secret once per repo (`gh secret
set CARGO_REGISTRY_TOKEN`) before the first tag push. If the
crate is internal-only and shouldn't go to crates.io, either drop
the `publish` job locally (release.yml is `when = "once"` so the
edit survives subsequent applies) or set `package.publish = false`
in `Cargo.toml`.

The binary name is derived from the GitHub repo name at runtime
(`${{ github.event.repository.name }}`), so the workflow is
identical across yukimemi/* CLIs unless your `[[bin]] name` in
`Cargo.toml` deliberately differs from the repo name — in that
case override `BIN_NAME` in the workflow's `env:` block.
<!-- kata:agents:rust-cli:end -->
