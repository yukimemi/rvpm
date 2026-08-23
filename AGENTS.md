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
# Supply-chain cooldown (minimum release age, like npm/pnpm's
# minimumReleaseAge): `rvpm update` won't advance to a commit until rvpm has
# first observed it for this long (or the commit itself is older than the
# window). ON BY DEFAULT at "1d" (pnpm 11 parity); set "0" to disable.
# Per-plugin override via `[[plugins]] cooldown` ("0" opts out); bypass once
# with `rvpm update --no-cooldown`. Observations live in
# `<cache_root>/cooldown_state.json`. See docs/architecture.md.
# cooldown = "1d"
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
#   "claude" / "agy" / "codex" / "opencode" — spawn the corresponding CLI as a subprocess
#   "gemini" — deprecated; Gemini CLI was retired for Free / AI Pro / Ultra
#              personal accounts on 2026-06-18 and "agy" (Antigravity CLI) is
#              its successor. Still works with paid Google Cloud API keys.
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
# cooldown: per-plugin supply-chain cooldown override (see options.cooldown)
# cooldown = "7d"      # stricter for this plugin; "0" opts it out
# build: shell command (run after sync / update completes, 5 min timeout)
# build = "cargo build --release"
# build_lua: Lua snippet executed via nvim --headless -u NONE -l (#97)
# Appends self + transitive depends to rtp; stdpath() reflects the real env, so
# native lib installs (e.g. blink.cmp) land properly in the user's data dir.
# build_lua = "require('blink.cmp').build():wait(60000)"
# setup: the plugin's setup() call. THE PRESENCE OF THE FIELD IS THE SWITCH — rvpm
# emits `require("<module>").setup(<opts>)` for this plugin; omit the field and rvpm
# never calls setup (hooks stay in charge). The value is turned into a Lua table
# literal at generate time, so it can hold data only — callbacks / `vim.*` calls
# belong in after.lua. Order: setup -> after.lua.
# setup = {}                                       # call setup with no options
# setup = { defaults = { layout_strategy = "vertical" } }   # the table IS the options
# setup = { main = "mini.pick", opts = { n = 7 } } # descriptor: name the module to
#   require. A table whose keys are only `main` / `opts` is read as this descriptor;
#   normally omit it because rvpm resolves the module from the plugin's `lua/` tree at
#   generate time and warns + skips just that setup call when it cannot decide. Options
#   literally named `main` go through `setup = { opts = { main = ... } }`.
#   One entry = one setup call; repos needing several (mini.nvim etc.) keep the extra
#   calls in after.lua (rvpm#358).

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
# Conditional loading (Lua expression, runtime)
cond = "vim.fn.has('win32') == 1"
# Compile-time exclusion (#140). Tera-rendered at generate/sync; a truthy
# result ("true"/"1"/"yes"/"on", case-insensitive) keeps the plugin, anything
# else drops it ENTIRELY — no clone, no merge, no loader.lua, no dep resolution.
# `when` (compile-time) and `cond` (runtime) compose: when is checked first.
# when = "{{ is_windows }}"            # or {{ env.RVPM_ENABLE_DEV }} / {{ vars.enable_custom }}
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

**`setup` vs `after.lua`.** Data-only `setup({ ... })` belongs in the entry's
`setup` field — rvpm then calls `require("<module>").setup(<opts>)` itself, right
before this plugin's `after.lua`. Anything that needs a Lua function (callbacks,
keymaps, autocmds, `vim.*` calls) has no TOML representation and stays in
`after.lua` — and when a *single* setup call needs both data and a Lua value, the
whole call stays in `after.lua` with no `setup` field, because splitting it would
mean setting up the same module twice. Never set up the **same module** in both
places — rvpm warns about the double setup at generate time (setting up a
different module from a hook is fine).

At generate time rvpm checks each file's existence and embeds `dofile(...)` in loader.lua only for ones that exist (pre-compiled).

## Architecture overview

The crate is **library + bin**: `src/main.rs` is a thin shell whose `main()` just calls `rvpm::run()`, and `src/lib.rs` hosts the CLI definitions, the command handler, and every helper. `run()` builds the Tokio runtime and dispatches the parsed CLI; each command is implemented as a `run_*()` function that runs on that async runtime. Keeping the logic in the library crate is what lets `cargo test --doc` and unit tests reach it (#176).

```text
src/
  main.rs       — thin binary entry point: `fn main() { rvpm::run() }`
  lib.rs        — CLI definitions (clap), `run()` entry point, run_*() implementations for every command, helper functions
  config.rs     — TOML config parsing (with Tera template expansion), MapSpec type, sort_plugins
  cooldown.rs   — supply-chain cooldown: `<cache_root>/cooldown_state.json` の read/write と「この commit へ進んでいいか」の pure 判定
  doctor.rs     — `rvpm doctor` — 17 diagnostics × 4 categories + render (nerd/unicode/ascii)
  git.rs        — async wrappers for git clone/pull/fetch/checkout (Repo struct) + GitChange recording
  helptags.rs   — runs :helptags via nvim --headless to generate tags
  link.rs       — file-level linking into the merged directory (hard link, first-wins on conflict); `placed` returns newly placed files for winner tracking
  loader.rs     — logic that generates Neovim's loader.lua
  merge_conflicts.rs — read/write of `<cache_root>/merge_conflicts.json` (most recent sync only; consumed by doctor)
  lockfile.rs   — read/write of `<config_root>/rvpm.lock` (reproducible plugin versions; intended to be committed to dotfiles)
  tui.rs        — ratatui-based progress / list display TUI
  update_errors.rs — read/write of `<cache_root>/update_errors.json` (per-plugin last-failed `rvpm update`; surfaced by `rvpm list` as `UpdateFailed`, cleared by a later successful update/sync)
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

`repos/<plug>/` is **never on rtp** — only `merged/` and `views/<plug>/` are. Conflicts are first-wins, recorded in `merge_conflicts.json` (self-conflicts filtered), surfaced by `rvpm doctor`. `cond` plugins get `merge=false` forced by `disable_merge_if_cond`, and `merge_doc=None` is forced to `Some(false)` (explicit per-plugin `Some(true)` survives — Windows-only-but-help-findable use case). Rebuilds are **incremental**: `.rvpm-stamp.json` (clone HEAD + merge mode, `src/view_stamp.rs`) lets unchanged views / `merged/` / helptags be skipped; `generate --force` / `sync --rebuild` bypass the skip. Full rules in [`docs/architecture.md`](docs/architecture.md).

### Path conventions

Config / cache are **fixed at `~/.config/rvpm/` and `~/.cache/rvpm/` across all platforms** (no `%APPDATA%` on Windows — keeps dotfile layouts identical across WSL / Linux / Windows).

- `cache_root` (override: `options.cache_root`) → default `~/.cache/rvpm/<appname>` — moves repos / merged / views / loader.lua together.
- `config_root` (override: `options.config_root`) → default `~/.config/rvpm/<appname>/plugins` — per-plugin init/before/after.lua.
- `<appname>` resolves as `$RVPM_APPNAME` → `$NVIM_APPNAME` → `"nvim"`.

Always go through the `resolve_*` helpers in `src/lib.rs` — never hardcode `.config/rvpm/...` or `.cache/rvpm/...` string literals. Full table of helpers and the directory layout in [`docs/architecture.md`](docs/architecture.md).

### Windows support

Plugin contents are merged with file-level hard links (`std::fs::hard_link`) on NTFS — no admin rights required. Falls back to `std::fs::copy` for cross-volume cases.

The single exception is the per-view `.git` indirection: `views/<plug>/.git` is a directory junction (Windows; created via the `junction` crate, no `mklink` cmd-spawn) or symlink (Unix) pointing to the plugin's clone `.git` dir. Junctions also need no admin rights. This is required for plugins that detect their own git state from the rtp dir (e.g. blink.cmp's `vim.fs.root('.git')` + `git describe --tags`).

## CLI commands

`rvpm sync / generate / clean / add / tune / update / remove / edit / set / config / init / list / browse / doctor / profile / log / completion`. Full flag-by-flag reference and the contributor checklist for adding flags is in [`docs/cli.md`](docs/cli.md).

## First-run support

`rvpm sync` / `rvpm generate` call `print_init_lua_hint_if_missing()` at the end and print guidance when Neovim's `init.lua` (resolved with `$NVIM_APPNAME`) does not reference loader.lua (or has not been created yet). Running `rvpm init --write` then either creates init.lua if absent or appends to its end (idempotently). The insertion is annotated so it is clearly identifiable as "added by rvpm."

<!-- kata:agents:base:begin -->
## Shared conventions

This file is the agent-agnostic source of truth (per the
[agents.md](https://agents.md) convention). The matching
`CLAUDE.md` and `GEMINI.md` files are thin shims that point back
here so each tool's auto-load behaviour still finds something.
**Edit AGENTS.md, not the shims.**

### Git workflow

- **No direct push to `main`.** Open a PR.
  - Exception: trivial typo / whitespace / docs wording fixes.
- Branch names: `feat/...`, `fix/...`, `chore/...`.
- **PR titles + bodies in English. Commit messages in English.**
- **Releases are PR-driven, tagging is automatic.** Bump
  `[workspace.package].version` (workspace) or `[package].version`
  (single crate) in a `chore/release-vX.Y.Z` PR. On merge to `main`,
  `.github/workflows/auto-tag.yml` (kata-managed) detects the bump,
  pushes the `vX.Y.Z` tag, and that tag fires `release.yml` for
  binary builds + crates.io publish. **Do not run `git tag` by
  hand** — the bot tag will collide and the manual push fails.

### PR review cycle

- Every PR runs reviews from **Claude Code**
  (`.github/workflows/claude-review.yml`, kata-managed) and
  **CodeRabbit**. Wait for both bots to post, address their
  comments (push fixes to the PR branch), and merge only after
  feedback is resolved. The claude-review workflow skips
  review-exempt PRs by itself (its job-level `if:` excludes
  `chore/release-*`, `kata-apply/auto`, `apm-bump/auto`, and
  Renovate / Dependabot authors) — a missing Claude review on
  those PRs is expected, not a failure.
- **Any PR that touches the Claude workflow files goes
  unreviewed.** `claude-code-action` requires the workflow file to
  already exist on the default branch **with identical content** —
  otherwise a PR could rewrite the workflow to exfiltrate the
  token. When the content differs it logs "Skipping action due to
  workflow validation" and exits 0 without reviewing: a green
  check with no review attached. This covers two cases, and the
  second is the one that keeps surprising people:
  - the PR that first adopts these templates (the workflow does
    not exist on the default branch yet), and
  - any later PR that **edits** `claude-review.yml` / `claude.yml`,
    e.g. hand-pulling an upstream template fix.

  Not fixable from this side — it is the mechanism that makes the
  token safe to hand to the action at all. Expected: merge on CI +
  owner approval; reviews resume on the next PR that leaves the
  workflows alone. The `kata-apply/auto` branch is already excluded
  by the job-level `if:`, so the daily template-refresh PRs do not
  add noise here.
- **A missing credential fails loudly instead.** If the repo has
  neither `CLAUDE_CODE_OAUTH_TOKEN` nor `ANTHROPIC_API_KEY` set,
  the guard step fails the job — set one and re-run (subscription
  path: `claude setup-token` → `gh secret set`; pay-as-you-go:
  store `ANTHROPIC_API_KEY` and swap the action input to
  `anthropic_api_key`). Distinguishing the two: **red** means no
  credential, **green with no review** means workflow validation.
- **The Claude full review fires once, at PR open** (plus
  `ready_for_review` / `reopened`) — fix pushes do **not** re-trigger
  it (`synchronize` is deliberately off the trigger list; a full
  re-review per push doubled up with the mention-driven re-check
  below and burned tokens for no extra signal). Verification of
  fixes rides the `@claude` thread replies. After a large rework
  that changes the PR's shape, request a fresh full pass
  explicitly: `@claude please re-review the full PR`. CodeRabbit
  still reviews pushes on its own cadence (its app config, not
  this workflow).
- **After opening a PR, immediately enter the review-monitoring
  loop — do not ask the user whether to start it.** Drive the
  cadence with `/loop` — fixed-interval mode (e.g.
  `/loop 60s …`) schedules ticks via `CronCreate`; dynamic mode
  (no interval, `/loop …`) self-paces via `ScheduleWakeup`. The
  agent actively pulls fresh state each tick with
  `gh pr view <N> --json state,reviews,comments,statusCheckRollup`
  and `gh api repos/<owner>/<repo>/pulls/<N>/comments` (the
  latter covers inline review comments, which `gh pr view`
  does not surface) and reacts to new bot feedback. Passive
  watchers (background `gh` polls, file watchers, hooks) cannot
  trigger active follow-up, so they are not a substitute —
  without an active wake-up the agent never re-reads the PR.
- **Default polling interval: 60s.** Claude Code review /
  CodeRabbit typically reply within ~1–5 minutes of a push or
  thread reply, so a 60s tick catches them on the next wake-up
  without burning cache: 60s sits well inside the 5-minute
  prompt-cache TTL, so the conversation context stays cached
  across ticks. Do **not** stretch the interval to 300s — that
  is the worst-of-both window (you pay the cache miss without
  amortizing it). If the PR is idle but a bot re-review is still
  expected (e.g. a CodeRabbit rate-limit refill window), step
  **up** to 1200–1800s instead.
- **Stop the loop entirely when only owner approval is missing.**
  Once review bots are quiet (or quiet-by-exception — version-bump
  skip, Renovate/Dependabot skip), CI is green, and there is no
  other expected follow-up, the *only* remaining action is human
  approval. GitHub already notifies the owner; the agent
  re-entering on every cron tick to find the same "still waiting
  on owner" state burns cache and adds no value. Stop scheduling
  further wake-ups (`CronDelete` in fixed-interval mode; simply
  omit the next `ScheduleWakeup` in dynamic mode) and report the
  wait state to the user. The owner restarts the loop after their
  next push if a fresh bot pass is wanted, or merges directly.
  (A CodeRabbit rate-limit window doesn't qualify on its own — a
  re-review is still expected once the quota refills, so step up
  to 1200–1800s instead and let it ride. Stopping is only correct
  when the owner has explicitly chosen to skip the bot pass per
  the rate-limit exception below.)
- **Reply to reviewers after pushing a fix — in each thread, not
  at the top level.** Every finding lives in its own inline review
  thread; answer *each* one as an in-thread reply, carrying an
  **@-mention** (`@claude` / `@coderabbitai`). Use the review-
  comment *replies* endpoint — `gh api repos/<owner>/<repo>/pulls/<N>/comments/<comment_id>/replies -f body=…`
  (or `-F in_reply_to=<comment_id> -f body=…` on the comments
  endpoint — `body` is required there too) — and
  get each comment's `<comment_id>` from
  `gh api repos/<owner>/<repo>/pulls/<N>/comments`. A single
  top-level `gh pr comment` does **not** count: it leaves every
  inline thread unresolved, the bot can't tie your response to the
  finding it raised, and the per-finding audit trail is lost.
  Reply in-thread even when you're **declining** a suggestion —
  say why; a silent skip reads as overlooked. Note `@claude` also
  triggers the interactive responder
  (`.github/workflows/claude.yml`, kata-managed) — it will
  re-check the fix and reply on the thread. Since fix pushes no
  longer re-trigger the full review, this mention-driven re-check
  is the **only** Claude-side verification of a fix — don't skip
  it for substantive fixes; do skip it for pure FYI notes that
  need no verification.
- A review thread is **settled** the moment the latest bot reply
  is ack-only ("Thank you" / "Understood" / a re-review summary
  with no new findings) or 30 minutes elapse with no actionable
  comment.
- **Merge gate**: review bots quiet AND owner explicit approval.
- Bot-authored PRs (Renovate / Dependabot) skip the bot-review
  gate; CI green + owner approval is enough.
- **Version-bump-only PRs** (a single `chore/release-vX.Y.Z`
  branch whose entire diff is `[workspace.package].version` /
  `[package].version` + the matching inter-crate refs +
  `Cargo.lock`) **also skip the bot-review gate.** There is
  nothing for the bots to find in a version bump, and the
  release pipeline downstream of merge (auto-tag → release.yml)
  is time-sensitive. CI green + owner approval is enough.
- **Treat CodeRabbit rate-limit notices as "quiet" for the
  merge gate.** If CodeRabbit only posts a "Review limit
  reached" quota-exhaustion message (no findings, no inline
  comments), it has produced no review content — there is
  nothing to address. Re-trigger with `@coderabbitai review`
  once the quota refills if you want a real pass; for small or
  time-sensitive PRs, merge on owner approval without waiting.

### Worktree workflow

> **Before your FIRST edit to any file, run `renri add` — NEVER edit the
> main checkout.** Read-only inspection (Read / Grep / Glob) stays on the
> main checkout; the instant you intend to *change* a file, you must
> already be in a worktree. The trap that keeps catching agents: diving
> into a fix the moment the diagnosis lands and editing in place. A
> concurrent agent shares the main checkout — your in-place edits will
> clobber theirs or be clobbered, and in a jj-colocated repo a stray
> working-copy commit entangles unrelated WIP into your branch. If you
> slip and edit in the main checkout, capture the diff first (jj already
> snapshotted it into the working-copy commit, so `jj diff > patch`; for
> git, `git stash` or save a patch — if you got as far as committing on a
> branch, just push it). Then reset the main checkout to pristine main
> (`jj new main@origin`, or `git switch -`), `renri add` a worktree, and
> re-apply the captured diff there.

Use [`renri`](https://github.com/yukimemi/renri) for any
commit-bound change. From the main checkout:

```sh
renri add <branch-name> --from main@origin            # create a worktree (jj-first), off latest upstream main
renri --vcs git add <branch-name> --from origin/main  # force a git worktree, off latest upstream main
renri remove <branch-name> -y --non-interactive  # cleanup after merge (agent-safe; see note)
renri prune                        # GC stale worktrees
```

Read-only inspection can stay on the main checkout.

**Always pass `--from <upstream main>`** (`main@origin` for jj,
`origin/main` for git). Without it, `renri add` forks off the *cwd
worktree's current HEAD* — in a long-lived main checkout that often
lags upstream, so the PR later shows up CONFLICTING against a `main`
that had already moved (e.g. a refactor merged upstream before the
branch was cut), forcing a manual re-port of the whole change.
`renri add` does fetch first, but fetching only updates `main@origin`
— it never moves the checkout's HEAD, so an explicit `--from` is what
guarantees a fresh base.

**Agents / non-interactive shells:** `renri remove` prints a details
panel and waits for a confirmation prompt — without `-y` it **hangs**,
and `--non-interactive` *alone* errors asking for `-y`. Always pass
`-y`, and add `--non-interactive` so a mistyped/omitted name fails
instead of opening a fuzzy picker (the same picker-fallback applies to
`remove` / `cd` / `exec` with no name). Use `-f`/`--force` to remove a
worktree that still has uncommitted changes or conflicts. To sweep
every merged-PR worktree in one shot: `renri remove --merged -y`.

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

This repo follows the shared Rust toolchain conventions. The
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
fix to `yukimemi/pj-rust` so every Rust project using these templates picks
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

### Releasing: version bump PR + auto-tag

Releases are triggered from `main` by a Cargo.toml version
change. `.github/workflows/auto-tag.yml` is kata-managed (source:
`yukimemi/pj-rust/.github/workflows/auto-tag.yml.tera`). It
watches `main` and, whenever a commit lands that changes the
top-level `version = "..."` in `Cargo.toml`, it pushes a matching
`vX.Y.Z` tag — no manual `git tag` step is needed. The tag push
then fires `release.yml`; see `kata:agents:rust-lib:*` or
`kata:agents:rust-cli:*` for what release.yml does in each
crate shape.

Cut a release via a small PR — never `git push` the bump
straight to `main`, even though the base block lists version
bumps as an exception to "no direct push". `auto-tag.yml` only
fires on `main`-branch pushes, so the bump must land via a merge
either way; using a PR also gives CI a chance to gate the
release. Enable automerge so CI green = release start:

```sh
git switch -c chore/release-vX.Y.Z
# Edit `package.version` in Cargo.toml, then:
cargo build                     # let Cargo.lock follow
git commit -am "chore: release vX.Y.Z"
git push -u origin chore/release-vX.Y.Z
gh pr create --fill
gh pr merge --auto --squash --delete-branch
```

Once CI is green the PR auto-merges. `auto-tag.yml` then pushes
`vX.Y.Z`, which fires `release.yml`.

**In a workspace, the version is in more than one place.** A member
that is published and depended on by another member is declared
with both a `path` and a `version` — crates.io needs a
requirement it can resolve for somebody who is not building from
the checkout, so a bare `path` will not do:

```toml
my-core = { path = "crates/my-core", version = "0.4.2" }
```

That literal does not follow `[workspace.package] version`.
Nothing in Cargo makes it, and the release above will not either.

**It fails late and quietly.** `version = "0.4.2"` means `^0.4.2`,
so a stale pin keeps resolving through every *patch* release and
stops only at the first bump that crosses the minor — where
`cargo build` refuses with `candidate versions found which didn't
match`, in the middle of cutting the release. Two repos on these
templates hit exactly this, one of them three releases after its
pins were last correct, and the other had already written the
hazard down in prose and drifted anyway.

So bump the pins in the same commit, keep them in
`[workspace.dependencies]` rather than in each member, and assert
it rather than remembering it. A test is the cheapest place —
`cargo test` already runs in CI, and it needs no toolchain a Rust
workspace does not have. [pj-rust-workspace's
README](https://github.com/yukimemi/pj-rust-workspace#the-internal-version-pin-and-the-check-for-it)
carries one to copy into any member's
`tests/check_versions.rs`: `internal_pins_match_the_workspace_version`
fails when a pin and the workspace version disagree, and
`members_inherit_the_workspace_version` fails when a member writes
its own version or reaches for a sibling by path.

**Repo settings to set once:** enable
`delete_branch_on_merge=true` (Settings → General →
"Automatically delete head branches"). The `--delete-branch`
flag on `gh pr merge --auto` is effectively a no-op — gh
returns as soon as automerge is enabled, so the deletion has to
happen server-side, which requires the repo setting.

**Why `KATA_APPLY_TOKEN`:** GitHub refuses to fire downstream
workflows from tags pushed by the default `GITHUB_TOKEN`, so
`auto-tag.yml` pushes with `KATA_APPLY_TOKEN` (the same PAT
`kata-apply.yml` already uses). Each consumer repo needs a
`KATA_APPLY_TOKEN` secret set; if a version-bump merge silently
doesn't fire `release.yml`, the missing PAT is the first thing
to check.
<!-- kata:agents:rust:end -->
<!-- kata:agents:rust-cli:begin -->
### Rust CLI release flow

This is a Rust CLI crate, so the release pipeline is publish-aware.
`yukimemi/pj-rust-cli` ships a tag-driven release workflow in
`.github/workflows/release.yml` (rendered from
`release.yml.template` for the same don't-auto-execute reason
ci.yml uses).

Releases are triggered by a Cargo.toml version bump landing on
`main`. The bump flow itself (PR with automerge → `auto-tag.yml`
pushes `vX.Y.Z` → `release.yml` runs) is documented in
`kata:agents:rust:*` under "Releasing: version bump PR +
auto-tag" — that block also covers the `KATA_APPLY_TOKEN` and
`delete_branch_on_merge` setup. What `release.yml` then does for
a **CLI** crate:

1. Cross-compiles binaries for **three** targets — full triples
   `x86_64-unknown-linux-musl`, `x86_64-pc-windows-msvc`,
   `aarch64-apple-darwin`. Linux is musl (statically linked, so the
   binary runs on any glibc vintage); the Linux job installs
   `musl-tools` first. Intel Mac (`x86_64-apple-darwin`) is
   deliberately **not** built — Apple Silicon only.
2. Uploads them as a GitHub Release with auto-generated notes.
3. `cargo publish --locked` to crates.io using the
   `CARGO_REGISTRY_TOKEN` repo secret.

Set the `CARGO_REGISTRY_TOKEN` secret once per repo (`gh secret
set CARGO_REGISTRY_TOKEN`) before the first release. If the
crate is internal-only and shouldn't go to crates.io, either drop
the `publish` job locally (release.yml is `when = "once"` so the
edit survives subsequent applies) or set `package.publish = false`
in `Cargo.toml`.

The binary name is derived from the GitHub repo name at runtime
(`${{ github.event.repository.name }}`), so the workflow is
identical across CLIs using these templates unless your `[[bin]] name` in
`Cargo.toml` deliberately differs from the repo name — in that
case override `BIN_NAME` in the workflow's `env:` block.

### Release smoke target (`examples/smoke.rs`)

After `cargo build --release`, `release.yml` runs
`cargo run --release --target <T> --example smoke` on every build
matrix entry. `cargo test` runs only library code, so the produced
binary's startup path goes unverified — that's how shoka v0.10.0
shipped a rustls `CryptoProvider` panic to crates.io even though
all 13 CI checks were green.

The template's default `examples/smoke.rs` body is intentionally
no-op so kata can drop it into every consumer crate without
breaking releases. **Override it per crate** with the smallest
operation that exercises the regression-prone surface:

- HTTPS-using CLIs: build the API client (octocrab, reqwest, etc.)
  and issue a tiny no-auth GET — that forces the rustls handshake
  to run inside the same binary the release publishes.
- File-handling CLIs: write+read a temp file via the real I/O
  helpers (catches missing crate features, permission regressions).
- Pure library crates: leave as no-op.

A failing smoke blocks the release before publishing to GitHub
Releases / crates.io.
<!-- kata:agents:rust-cli:end -->
