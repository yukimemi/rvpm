# Architecture

In-depth design notes for rvpm. The high-level concept and quickstart for
Claude Code live in [CLAUDE.md](../CLAUDE.md); this file holds the long-form
rationale.

## loader.lua generation strategy (`src/loader.rs`)

rvpm performs **full control over plugin loading** + **merge optimization** +
**pre-glob at generate time**. Structure of loader.lua:

```text
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
             require("<module>").setup(<opts>)   ← only when `setup` is set
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

### `setup` → `setup()` (module resolution is AOT)

When a `[[plugins]]` entry carries `setup`, rvpm emits the plugin's
`require("<module>").setup(<opts>)` call itself. The **presence** of the field
is the switch; an absent field means rvpm never calls setup. Three forms:

```toml
setup = {}                                  # call setup with no options
setup = { notify = true }                   # the table IS the options
setup = { main = "mini.pick", opts = {} }   # descriptor: name the module
```

`config::interpret_setup` decides which form a value is: a table whose keys are
only `main` / `opts` is the descriptor, anything else is the options table
itself. `setup = {}` lands on the same result either way. Options that are
literally named `main` therefore go through the descriptor
(`setup = { opts = { main = … } }`), and a table that mixes `main` with other
keys gets a generate-time warning because it is nearly always a mistyped
descriptor. A non-table `setup` (e.g. `setup = true`) warns and is skipped.

**Where the call sits.** For an eager plugin it is emitted inside phase 6,
after `after/plugin/**` is sourced and **immediately before that plugin's
`after.lua`**. For a lazy plugin the identical slot exists inside `load_lazy`
(`if setup then setup() end` between the `after_plugin_files` loop and
`dofile(after)`), so it fires when the trigger fires. The order is therefore
always **setup → after.lua**: `after.lua` is the place that adds to or
overrides an already-set-up plugin.

**Resolution is generate-time, not startup-time.** `plugin_build.rs` decides
the module name and renders the options into a Lua table literal during
`generate` / `sync`, and loader.lua receives only the finished literal
(`rvpm_setup("<name>", "<module>", { ... })`). Startup cost is zero and an
unresolvable module is a generate-time warning instead of a runtime error.
`plugin_scan::resolve_main_module` decides in order:

1. explicit `setup = { main = "..." }` — used verbatim;
2. a `mini.xxx` display name (`mini.nvim` itself excluded) — the display name
   *is* the module, because those repos ship only `lua/mini/xxx.lua`;
3. the top-level modules under the plugin's `lua/` (`lua/<mod>.lua` and
   `lua/<mod>/init.lua` only — `lua/<mod>/<sub>.lua` is not `require`-able as
   `<mod>`) whose normalized name equals the plugin's normalized display name;
4. otherwise, the sole top-level module if there is exactly one
   (`vim-illuminate` → `illuminate`);
5. otherwise nothing — rvpm warns, asks for the descriptor form, and **skips
   only that plugin's setup call** (resilience: the rest of loader.lua is
   generated normally).

Normalization (`plugin_scan::normname`) is lowercase → strip leading `vim-` /
`nvim-` → strip trailing `.vim` / `.nvim` → remove `.lua` / `-lua` → drop every
non-`[a-z]` character. That folds `telescope.nvim` / `nvim-telescope` /
`which-key.nvim` onto the module names those repos actually expose.

**Lazy plugins build the table lazily.** The emitted setup for a lazy plugin is
wrapped in a closure (`local _rvpm_st_<name> = function() rvpm_setup(...) end`)
that is passed to `load_lazy`. Only the closure is constructed at startup; the
options table literal itself is never built if the trigger never fires. Lazy
deps loaded from inside a trigger callback get the same treatment.

**Failures stay local.** `rvpm_setup` (emitted next to the `load_lazy` helper,
and only when at least one plugin has `setup`) wraps the call in `pcall` and
reports through `vim.notify(..., vim.log.levels.ERROR)`. A plugin whose
`setup()` throws must not take down the remaining eager plugins or the phase 7
trigger registrations.

**Two guardrails at generate time.** (1) Unresolvable module → warn + skip, as
above. (2) Double setup: when `setup` is set *and* the plugin's `before.lua` /
`after.lua` calls `.setup` **on the same resolved module**, rvpm warns that
setup would run twice and asks the user to keep exactly one (`setup` for plain
data, the hook for anything needing Lua functions). The check binds the call to
the module — `require("<other>").setup{}` in a hook is a different module and
stays silent — and understands Lua's call sugar (`setup{...}`, `setup"..."`,
`setup[[...]]`), so neither form slips through nor produces false positives.

**Known limit.** `setup` is TOML, so it can only express data — strings,
numbers, booleans, arrays, nested tables. Callbacks (`on_attach = function()
... end`) and `vim.*` calls have no TOML form and belong in `after.lua`. One
entry also emits exactly one setup call; repos needing several (monorepos like
`mini.nvim`) put the extra calls in `after.lua` (see #358).

## Lazy trigger implementation

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

## Change history via update_log.json (`src/update_log.rs`)

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

## Reproducibility via rvpm.lock (`src/lockfile.rs`)

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

`rev` also accepts the `/regex/` form (same delimiter convention as `on_cmd`,
`on_event`, `on_map`). When matched, rvpm enumerates local tags after fetch,
filters by the regex, parses each as semver (after stripping a leading `v` /
`V`), and checks out the **highest semver match**. Tags that fail to parse as
semver are silently ignored (lazy.nvim-equivalent behavior). Pattern resolution
runs on every `sync` / `update` (no caching) — so `rev = "/^v1\\..*/"` behaves
like a moving pin that always tracks the latest matching tag. If you need
commit-level pinning, use a literal tag instead. `--frozen` does **not**
override pattern re-resolution; freeze a literal commit by switching to
`rev = "<sha>"` if strict reproducibility is required for that plugin.

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
- **Shallow-clone recovery** (`checkout_with_pin_fetch_retry` in `src/git.rs`):
  clones are depth 1 and fetches only `Deepen(1)`, so a lockfile pin more than
  ~2 commits behind the remote tip is absent from the local object DB — e.g.
  right after the user deletes `repos/` and re-syncs (fresh depth-1 clone vs.
  an old lockfile pin). When checkout fails for a full 40-hex SHA, rvpm fetches
  just that commit from origin (object-id want, equivalent to
  `git fetch origin <sha>`, done in-process via a one-sided gix refspec) and
  retries the checkout. Requires the server to allow SHA wants (GitHub allows
  it for reachable commits); if the fetch also fails, the original
  `rev '<sha>' not found` error is returned. Non-SHA revs (branch / tag /
  `/regex/`) never trigger the retry and fail as before.
- **Broken-clone recovery** (`sync_impl`): if `repos/<plug>/` exists but has no
  valid `.git` (e.g. partially deleted by hand), sync removes the leftover dir
  and re-clones instead of failing with "does not appear to be a git
  repository". The auto-remove applies to **remote URLs only** — a local-path
  dst (potential dev / mirror dir) is never deleted and keeps the old error.
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

## Supply-chain cooldown (`src/cooldown.rs`)

"Minimum release age" gate for `rvpm update`, **on by default at 1 day**,
mirroring what npm / pnpm shipped after the 2025 Shai-Hulud-class worm
attacks (pnpm 11 also defaults to 1 day) and what folke/lazy.nvim#2141
proposes for Neovim: a malicious commit is usually detected and reverted
within hours-to-days, so refusing to apply anything *too new* skips most
attack windows.

Configuration: `options.cooldown` (humantime-lite, shared parser with
`fetch_interval`), per-plugin `[[plugins]] cooldown` override. **Unset →
`DEFAULT_COOLDOWN` (1d, default on)**; `"0"` disables (globally or
per-plugin). Parse failures **fail closed** to 1d (a typo in a safety knob
must not silently disable it — note this is the opposite fallback direction
from `fetch_interval`, and `DEFAULT_COOLDOWN` serves as both the unset
default and the parse-failure fallback since both want 1d).

### Why first-seen instead of committer dates

Git has no registry-style trusted publish timestamp, and committer dates are
trivially backdatable by an attacker — gating on them alone would neuter the
whole mitigation. So the primary signal is **when rvpm itself first observed
a commit as the remote tip** (`first_seen`), recorded locally in
`<cache_root>/cooldown_state.json` on every fetch (both `sync` and `update`
record; the file is ephemeral cache — losing it just means every tip looks
freshly observed again, which fails safe). The committer date
(`committed_at`) is kept as a *secondary* eligibility branch so that a
dormant repo's months-old commit isn't pointlessly held on first contact;
that branch is the one acknowledged trade-off (backdating slips through it,
forging `first_seen` is impossible).

State schema (JSON, versioned like fetch_state):

```json
{ "version": 1,
  "entries": [
    { "name": "snacks.nvim", "url": "folke/snacks.nvim",
      "observed": [
        { "commit": "abc...", "first_seen": "2026-06-01T12:34:56Z",
          "committed_at": "2026-06-01T10:00:00Z" }
      ] }
  ] }
```

### Decision algorithm (`cooldown::decide`, pure)

A commit is **eligible** when `now - first_seen >= cooldown` OR
`now - committed_at >= cooldown` (unparseable / future timestamps count as
not eligible — clock skew fails safe).

For an update with remote tip `T` and current HEAD `H`:

1. cooldown disabled or `T == H` → **Advance** (plain no-op/update).
2. `T` eligible → **Advance** to `T`.
3. otherwise **Hold**: pick as fallback the eligible observed commit with the
   newest `first_seen` that is strictly newer than `H`'s own observation —
   this is what keeps an active plugin moving forward (always ~cooldown
   behind the tip) instead of freezing forever. If `H` has no observation
   entry, no fallback is taken (we cannot prove the candidate isn't a
   *downgrade*, so we stay put).

The fallback's `*seen > head_seen` guard uses `first_seen` (wall-clock
observation order) as a proxy for git ancestry. A second acknowledged
trade-off: a remote force-push can make observations arrive in an order that
doesn't match the rewritten git history, so a "newer by first_seen" fallback
could in principle be a git-ancestor of the current HEAD. In practice the
risk is tiny — force-pushes are rare, and a *malicious* force-push is exactly
what the cooldown is defending against (the rewritten tip is still held until
it matures) — but it's a real edge of the first-seen heuristic.

`cooldown::prune` keeps all pending (not-yet-eligible) observations, the
single newest eligible one, and the current HEAD's entry (the comparison
baseline), capped at 200. Entries with malformed `first_seen` sort before
valid ones (`Option<SystemTime>: None < Some`), so they are dropped first
when the cap fires — the safe direction.

### Integration points

- `update_single_plugin` (`src/lib.rs`): when the caller passes a
  `PluginCooldownCtx` (only for non-`rev`, non-dev plugins with an effective
  cooldown > 0), the update runs as `Repo::fetch()` →
  `Repo::remote_head()` → observe tip (+ `Repo::commit_time`) → `decide` →
  `Repo::reset_to_remote_tip()` (Advance) / `Repo::checkout_locally(sha)`
  (Hold-with-fallback; the sha is in the local DB because it was fetched
  when it was itself the tip) / no-op (Hold). Held plugins are listed in the
  update summary with tip age; `rvpm update --no-cooldown` skips the ctx
  entirely for one run.
- `run_sync` (`src/commands/sync.rs`): **not gated** — sync already honors
  lockfile pins, which is precisely what blocks new upstream commits between
  updates. But sync records tip observations on every real fetch (reusing
  the `remote_head` it already reads for the held-back summary, or
  `head_commit` on the fresh-clone/no-lock path where HEAD == tip), so tips
  mature even if the user rarely runs `update`.
- Exemptions: explicit `rev` (user's pin wins), `dev` plugins, first install
  (clone takes the tip; you have to start somewhere — the clone's HEAD is
  recorded as the first observation).
- URL mismatch between state entry and config (same display name, different
  repo) discards the observation history, same trust rule as the lockfile.

## Automatic helptags generation (`src/helptags.rs`)

On `sync` / `generate` completion, launch `nvim --headless --clean -c "source <tmp.vim>" -c "qa!"` once and run `:helptags <path>` against every target `doc/`. Disable via `options.auto_helptags = false`.

Why not embed it in loader.lua: rvpm's concept is to **prioritize Neovim startup speed above all else**. Generating helptags incurs an nvim process startup cost, so it is performed up-front on the rvpm side (sync/generate) rather than at Neovim startup.

Rules used by `collect_helptag_targets` to enumerate target `doc/`:
- If `merged_dir/doc/` exists, add it first — docs of merge=true & !lazy plugins are aggregated in one place, so a single `:helptags` call processes all of them.
- **Lazy plugins must be added individually even when merge=true** — `decide_merge_mode` (`src/lib.rs`) keeps lazy plugins out of merged/, so each plugin's own `doc/` must be processed.
- Eager plugins with merge=false are also added individually.
- `cond` is evaluated at Lua runtime and cannot be judged from Rust, so all plugins are candidates (= those visible in `rvpm list` = targets).

Working around command-line argument length: to avoid hitting Windows' `CreateProcess` limit (~8KB), instead of stringing `-c "helptags d1" -c "helptags d2" ...` together, the tool writes a Vim script (wrapped in `try/catch`) to a tempfile and sources it in one go via `-c "source <tmp>"`.

Resilience: if `nvim` is not on PATH, only a warning is emitted and rvpm continues. Even if the nvim process exits non-zero, Ok is returned. Duplicate-tag warnings from `:helptags` (E154 etc.) are passed through to stderr — they carry value as an improvement signal for users who explicitly opt into merge, so they are not suppressed.

## Parallel execution and Semaphore

`run_sync()` and `run_update()` spawn parallel tasks via `tokio::task::JoinSet`. When `config.options.concurrency` is set, task count is bounded by `tokio::sync::Semaphore`. `run_list()`'s background status check uses the same bound: `Repo::get_status()` runs on `spawn_blocking`, so an unbounded fan-out would start one OS thread per plugin and have them fight over the disk.

```rust
let concurrency = resolve_concurrency(config.options.concurrency);
let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
// At the top of each task:
let _permit = sem.acquire_owned().await.unwrap();
```

## `rvpm list` TUI latency (`src/tui.rs::HookCache`, `src/commands/list.rs`)

The list TUI used to rebuild every row on a fixed 50ms tick, and each row ran
three `exists()` calls for `init.lua` / `before.lua` / `after.lua` (the `I B A`
column). At 233 plugins that is ~700 stats per frame — measured at ~50ms warm
and ~150ms under Windows real-time scanning, so `j` / `k` visibly lagged: the
redraw monopolised the loop and key events queued behind it.

Two changes fix it, both in the "don't recompute what didn't change" family:

- **`HookCache::scan()`** walks the hooks once per config load (startup and
  after every `reload!()`), and `draw_list` only reads the cached `HookFlags`.
  Frame cost drops to ~2.7ms for 233 plugins.
- **Dirty-flag redraw.** The list TUI has no time-driven element (no spinner),
  so it draws only when something moved: a key press, a `Resize` event, or a
  status message arriving from the background check. Idle frames cost nothing.

The sync / update progress TUI (`TuiState::draw`) keeps its unconditional
redraw — its spinner and elapsed-time display are time-driven.

## TOML config templating

`parse_config()` parses in two passes: first extract the vars section only → register `vars`, `env`, `is_windows` into a Tera context → render the entire TOML string → final parse. This makes `{{ vars.base }}` and `{{ env.HOME }}` usable inside the config file.

## Flexible schemas (`string | string[]` / `MapSpec` / etc)

`deserialize_string_or_vec` and `deserialize_map_specs` in `config.rs` use `serde(untagged)` enums to accept multiple TOML shapes.

- Both `on_cmd = "Foo"` and `on_cmd = ["Foo", "Bar"]` are OK.
- Both `on_map = ["<leader>f"]` and `on_map = [{ lhs = "...", mode = ["n", "x"] }]` are OK.

The write side (`set_plugin_list_field`) writes back as a string for one element and as an array for multiple (the minimal representation).

## Merge strategy (`src/link.rs` + `src/lib.rs::decide_merge_mode`)

rvpm builds **at most two** rtp source directories (#119):

- `<cache_root>/plugins/merged/` — single shared rtp entry for Full-merged plugins (and the doc tag store).
- `<cache_root>/plugins/views/<host>/<owner>/<repo>/` — per-plugin rtp view; doc-stripped or doc-included depending on `merge_doc`.

The plugin clone at `<cache_root>/plugins/repos/<host>/<owner>/<repo>/` is **never** on rtp. Anything that needs to be reachable at runtime is hard-linked into one of the two locations above.

### `PluginMergeMode` (per plugin)

`decide_merge_mode(plugin.merge, plugin.lazy, plugin.merge_doc, options.merge_doc)` returns one of:

| Result            | Sync output                                                                 | rtp at runtime                                                  |
|-------------------|-----------------------------------------------------------------------------|-----------------------------------------------------------------|
| `Full`            | `merged/` aggregates every rtp dir of the plugin                            | `merged/` (appended once at startup)                            |
| `ViewWithDoc`     | `views/<plug>/` aggregates every rtp dir **including `doc/`**               | `views/<plug>/` (eager: startup; lazy: at trigger via load_lazy)|
| `ViewWithoutDoc`  | `views/<plug>/` aggregates every rtp dir **except `doc/`**, plus `merged/doc/` collects the plugin's `doc/` files | `views/<plug>/` (no doc) + `merged/` (provides the doc tag store) |

Resolution rule:

- `merge=true && eager` → `Full` (per-plugin / global `merge_doc` is ignored — full merge already covers `doc/`).
- otherwise → `effective_merge_doc = plugin.merge_doc.unwrap_or(options.merge_doc)`:
  - `true`  → `ViewWithoutDoc`
  - `false` → `ViewWithDoc`

`disable_merge_if_cond` runs first as a pre-pass: when `cond` is set, `merge=true` is forced to `false`, and `merge_doc=None` is forced to `Some(false)` (explicit `Some(true)` survives — that's the "Windows-only plugin but help findable cross-platform" use case).

### Incremental rebuild stamps (`src/view_stamp.rs`)

Because views are hard-link trees, file *contents* track the clone automatically (shared inodes); only the file *set* can drift, and that only happens when the clone's HEAD moves. rvpm exploits this with on-disk stamps so repeat `generate` runs skip nearly all I/O:

- **Per-view stamp** — `views/<plug>/.rvpm-stamp.json` records `{schema, rvpm_version, fingerprint}` where the fingerprint is `<HEAD commit>:<merge mode key>`. When the expected stamp matches, the whole walk + hard-link pass for that view is skipped. The stamp is written into the temp dir *before* the atomic rename, so "stamp exists ⟺ that fingerprint's build completed".
- **Merged stamp** — `merged/.rvpm-stamp.json` fingerprints every contributing plugin `(name, commit, full|doc)` in config order (first-wins depends on order). On match, the `merged/` rebuild is skipped entirely. If any contributor's commit is unknown (dev plugin, non-git clone), skipping is disabled and `merged/` rebuilds every run (safe side).
- **helptags skip** — when `merged/` was skipped, zero views were rebuilt, and every helptag target already has a `tags` file, the `nvim --headless` helptags pass is skipped too.
- **Always rebuilt** — `dev = true` plugins (local edits don't move HEAD) and clones without a readable HEAD never get a stamp. `rvpm generate --force` and `rvpm sync --rebuild [QUERY]` bypass the skip check but still write fresh stamps (so the *next* normal run can skip). Stamps embed the rvpm version and a schema number; either changing invalidates all stamps.
- **merge_conflicts.json on skipped runs** — a run that skipped the `merged/` rebuild never recomputed first-wins, so the previous snapshot is *preserved* instead of being overwritten with an empty list (`rvpm doctor` would otherwise misreport "no conflicts").
- **Background reaping** — the replaced `*.rvpm-old` tree after an atomic view swap is deleted on a background thread (`DirReaper`); a `ReapGuard` instantiated at the top of `run_generate` / `run_sync` joins them on every exit path (including early `?` returns). Leftovers from an aborted process are cleaned by the existing `ensure_absent` / `prune_stale_views` self-healing.

In `run_generate`, per-plugin view builds run in parallel (bounded by `options.concurrency`, same as sync's git phase) since views never collide; the `merged/` build stays sequential in config order to keep first-wins deterministic. `build_plugin_scripts` (pre-glob + `plugin_scan`) is parallelized the same way.

### File-level link mechanics

`merge_plugin()` (and `merge_plugin_no_doc()` for the doc-stripped view, `merge_plugin_doc_only()` for the doc-only aggregation into `merged/doc/`) link into the destination directory **at file granularity**. Design highlights:

- **Files are hard-linked** (no admin rights required on Windows; stable on Unix). Same volume is required, but since repos / merged / views are all under `<cache_root>` this is fine. If hard-link fails (e.g. cross-volume), fall back to `std::fs::copy`. Junctions are directory-only and cannot be used for files. Symbolic links require admin rights on Windows and are therefore not used.
- **Directories are just created** (`create_dir_all`). The directory itself is a real directory; its contents are recursively linked file by file. The previous junction-per-directory scheme would, when multiple plugins place files under the same hierarchy (e.g. several cmp plugins sharing `lua/cmp/`), cause last-writer-wins overwrites and clobber earlier contents.
- **First-wins + conflict summary** — on conflict, the new file is skipped and a `MergeConflict { relative }` is collected. `MergeResult.placed` returns the list of files newly placed in this run, and lib.rs maintains a `HashMap<PathBuf, String>` to **look up the winner plugin name** (loser-only would not tell you "which plugin did it collide with?"). Self-conflicts (winner == loser, e.g. when a plugin promoted from `ViewWithoutDoc` to `Full` re-links its already-placed `doc/` files) are filtered out by `record_merge_result`. At the end of `run_sync` / `run_generate`, `print_merge_conflicts` groups results by plugin, displays each line on stderr with `(kept: <winner>)` appended, and overwrites `<cache_root>/merge_conflicts.json` each time. `rvpm doctor` reads the latter and surfaces it as a warning.
- **Files at the plugin root are ignored** — README.md / LICENSE / Makefile / package.json / *.toml and other meta files have no place on the rtp; they would only become noise that collides across plugins.
- **Directories at the plugin root are allow-listed to rtp conventions + denops** — `plugin/`, `lua/`, `doc/`, `ftplugin/`, `ftdetect/`, `syntax/`, `indent/`, `colors/`, `compiler/`, `autoload/`, `after/`, `queries/`, `parser/`, `rplugin/`, `spell/`, `keymap/`, `lang/`, `pack/`, `tutor/` (for `:Tutor`), and `denops/` (for denops.vim TypeScript plugins). `tests/` `scripts/` `examples/` `src/` etc. are unrelated to the rtp and are excluded.
- **Skip dotfiles at every level** (`.gitignore`, `.luarc.json`, `.editorconfig`, `.gitkeep`, etc.) — they are unrelated to Neovim startup, and at deep levels (e.g. `doc/.gitignore`) would just collide across plugins and add conflict-warning noise.

### `.git` exposure in views (`link_dotgit_into_view`)

The dotfile-skip rule above intentionally drops `.git/` during the merge walk. But some plugins detect their own git state from the rtp directory — blink.cmp, for example, calls `vim.fs.root(plugin_dir, '.git')` followed by `git describe --tags --exact-match HEAD` to decide whether the prebuilt fuzzy library matches the current tag. Without `.git` visible from the view path, that probe sees "not a git repository" and falls back to the Lua implementation with a warning.

`link_dotgit_into_view` adds an indirection at view root pointing back to the plugin clone's real `.git`:

- **Windows**: directory junction via the [`junction`](https://crates.io/crates/junction) crate (native `DeviceIoControl` + `FSCTL_SET_REPARSE_POINT`, no `mklink` cmd-spawn, no admin rights required).
- **Unix**: `std::os::unix::fs::symlink`.

This is the **only** non-hardlink filesystem operation in the merge pipeline; all other merging stays at file granularity. The link is created inside the `atomic_replace_view_dir` tmp builder so it lands together with the rest of the view via atomic rename. Plugins without `.git` (e.g. `dev = true`) silently skip. Failures are warned but never abort the sync (resilience).

This fix only applies to `ViewWithDoc` / `ViewWithoutDoc` modes. `Full` mode (`merge=true && eager`) cannot expose a single plugin's `.git` because `merged/` is shared across plugins; if such a plugin needs git-state self-detection, the user should set `merge = false` to route it through the View modes.

### View cleanup

`prune_stale_views()` walks `views/` after each `sync` and removes any
`<host>/<owner>/<repo>/` directory whose plugin no longer expects a view
(removed from config, or promoted from `View*` to `Full` by
`promote_lazy_to_eager`). `run_clean` re-derives the expected set from
config alone and applies the same sweep.

### Profile `--no-merge` (`force_unmerge=true`)

The loader's `force_unmerge` flag (set when `rvpm profile --no-merge` runs) skips the `merged/` rtp:append and emits an individual `vim.opt.rtp:append(plugin.path)` per plugin — using **clone path**, not view path. The merge state on disk is left untouched; only the emitted loader changes. This restores the pre-#119 baseline so the profiler can measure "no merge optimization" startup honestly. `:help` keeps working because the clone tree includes `doc/`.

## Windows support

Once the merge strategy switched to file-level hard links, the setup no longer requires admin rights on Windows and uses neither junctions nor symbolic links. `std::fs::hard_link` works on NTFS without admin. Directories are created with `create_dir_all`, so junctions are not needed. The symbolic-link permission issue is avoided.

## Path conventions (fixed + overridable)

Config / cache are **fixed at `~/.config/rvpm/` and `~/.cache/rvpm/` across all platforms**. Even on Windows, `dirs::config_dir()` (`%APPDATA%`) is not used. Reasons:

- Aligns with Neovim's convention (`~/.config/nvim`).
- Lets dotfiles share an identical path layout across WSL / Linux / Windows.
- A single mental model is enough.

### Path helpers (src/lib.rs)

| Helper | Purpose | Override |
|---|---|---|
| `rvpm_config_path()` | `~/.config/rvpm/<appname>/config.toml` | Per-appname, like the other roots |
| `resolve_cache_root(opt)` | `~/.cache/rvpm/<appname>` or tilde-expanded `opt` | `options.cache_root` |
| `resolve_repos_dir(cache_root)` | `{cache_root}/plugins/repos` | — |
| `resolve_merged_dir(cache_root)` | `{cache_root}/plugins/merged` | — |
| `resolve_views_dir(cache_root)` | `{cache_root}/plugins/views` (per-plugin rtp views, #119) | — |
| `resolve_plugin_view_dir(views_dir, plugin)` | `{views_dir}/<host>/<owner>/<repo>/` | — |
| `resolve_loader_path(cache_root)` | `{cache_root}/plugins/loader.lua` | — |
| `resolve_config_root(opt)` | `~/.config/rvpm/<appname>/plugins/` or `opt` | `options.config_root` |
| `expand_tilde(s)` | General-purpose helper that expands `~` / `~/...` / `~\...` to home dir | — |

Do not write `.config/rvpm/...` or `.cache/rvpm/...` as string literals in code. Always go through a helper.

### Resolution order

- **cache_root**: `options.cache_root` (tilde-expanded) → default `~/.cache/rvpm/<appname>`
- **config_root**: `options.config_root` (tilde-expanded) → `~/.config/rvpm/<appname>/plugins`
- **repos**: always `{cache_root}/plugins/repos/<canonical>/` (per-plugin override is `plugin.dst`)
- **merged**: always `{cache_root}/plugins/merged/`
- **views**: always `{cache_root}/plugins/views/<canonical>/` (#119 — per-plugin rtp view)
- **loader**: always `{cache_root}/plugins/loader.lua`

In other words, setting just `options.cache_root` moves repos / merged / views / loader.lua together. `options.config_root` overrides only the per-plugin init/before/after.lua location, and defaults to `~/.config/rvpm/<appname>/plugins/` next to config.toml.

## Directory layout (default)

| Path | Purpose |
|------|------|
| `~/.config/rvpm/<appname>/config.toml` | Main configuration file (per-appname, so `$RVPM_APPNAME` / `$NVIM_APPNAME` gives a fully separate plugin set) |
| `~/.config/rvpm/<appname>/before.lua` | Global before hook (phase 3, before all init.lua; auto-applied if present) |
| `~/.config/rvpm/<appname>/after.lua` | Global after hook (phase 9, after all lazy triggers are registered; auto-applied if present) |
| `~/.config/rvpm/<appname>/plugins/<host>/<owner>/<repo>/` | Per-plugin init/before/after.lua (override via `options.config_root`) |
| `~/.config/rvpm/<appname>/rvpm.lock` | Lockfile of plugin commit pins (override via `options.config_root`). Commit it with your dotfiles to reproduce on other machines. |
| `~/.cache/rvpm/<appname>/plugins/repos/<host>/<owner>/<repo>/` | Plugin clone destination (never on rtp; #119) |
| `~/.cache/rvpm/<appname>/plugins/merged/` | Full-merge target (eager + merge=true) and the doc tag store for `merge_doc=true` plugins |
| `~/.cache/rvpm/<appname>/plugins/views/<host>/<owner>/<repo>/` | Per-plugin rtp view (#119). Doc-stripped or doc-included depending on effective `merge_doc` |
| `~/.cache/rvpm/<appname>/plugins/loader.lua` | Generated Neovim loader |
| `~/.cache/rvpm/<appname>/plugins/merged/doc/tags` | Aggregated help tags (`:helptags merged/doc` covers Full + DocOnly plugins in one pass) |
| `~/.cache/rvpm/<appname>/plugins/views/<host>/<owner>/<repo>/doc/tags` | Per-plugin help tags for `ViewWithDoc` plugins (those that opted out of doc-merge) |
| `~/.cache/rvpm/<appname>/update_log.json` | Change history of `sync` / `update` / `add` runs (read by `rvpm log`, max 20 runs) |
| `~/.cache/rvpm/<appname>/merge_conflicts.json` | Snapshot of merge conflicts from the latest `sync` / `generate` (read by `rvpm doctor`). Not history — overwritten each run. |

`<appname>` is determined as `$RVPM_APPNAME` → `$NVIM_APPNAME` → `"nvim"`, in that order. Setting `options.cache_root` moves the entire `~/.cache/rvpm/<appname>/` (repos/merged/loader.lua). `options.config_root` independently moves the per-plugin config directory.

## First-run support

`rvpm sync` / `rvpm generate` call `print_init_lua_hint_if_missing()` at the end and print guidance when Neovim's `init.lua` (resolved with `$NVIM_APPNAME`) does not reference loader.lua (or has not been created yet). Running `rvpm init --write` then either creates init.lua if absent or appends to its end (idempotently). The insertion is annotated so it is clearly identifiable as "added by rvpm."
