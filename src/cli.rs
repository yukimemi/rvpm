//! CLI surface: the clap `Cli` / `Commands` definitions and the `run()` /
//! `run_cli()` dispatch, extracted from `lib.rs` (#233). `pub use cli::run;`
//! in the crate root keeps `rvpm::run()` and `src/main.rs` unchanged.

use crate::commands::*;
use crate::self_update::*;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
// Clap 4 styling: section headers / usage / literals / placeholders を色分けする。
// `const` で渡せるようにビルダ経由で作成 (clap 4.5+ は Styles::styled() が const)。
const CLI_STYLES: clap::builder::styling::Styles = {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
        .usage(AnsiColor::BrightGreen.on_default().effects(Effects::BOLD))
        .literal(AnsiColor::BrightBlue.on_default().effects(Effects::BOLD))
        .placeholder(AnsiColor::Magenta.on_default())
        .error(AnsiColor::BrightRed.on_default().effects(Effects::BOLD))
        .valid(AnsiColor::BrightGreen.on_default())
        .invalid(AnsiColor::BrightYellow.on_default())
};

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Fast Neovim plugin manager with pre-compiled loader and merge optimization",
    long_about = "\
rvpm clones plugins in parallel, links merge=true plugins into a single\n\
runtime-path entry, and pre-compiles a loader.lua that sources everything\n\
without runtime glob cost. Inspired by lazy.nvim but adds merge and\n\
ahead-of-time file-list compilation on top.\n\
\n\
Run `rvpm init --write` once after your first `rvpm sync` to wire the\n\
generated loader.lua into your Neovim init.lua.",
    styles = CLI_STYLES,
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Clone/pull plugins and regenerate loader.lua
    ///
    /// With --prune, also delete any plugin directories under the repos
    /// cache that are no longer referenced by config.toml.
    Sync {
        /// Delete unused plugin directories after syncing
        #[arg(long)]
        prune: bool,
        /// Error out if any non-dev plugin is missing from rvpm.lock
        /// (strict reproducibility for CI / fresh machines)
        #[arg(long)]
        frozen: bool,
        /// Ignore rvpm.lock entirely: pull latest and do not write the lockfile
        #[arg(long)]
        no_lock: bool,
        /// Run each plugin's `build` command even when git HEAD did not move.
        /// By default `sync` skips build for plugins whose pull was a no-op,
        /// which makes "nothing changed" syncs much faster. Use this when you
        /// need to rerun e.g. `:TSUpdate` or a manual rebuild step.
        ///
        /// Accepts an optional query to limit rebuild scope to plugins whose
        /// `url` or `name` contains the substring. Useful when iterating on a
        /// single plugin's `build` command:
        ///
        ///   rvpm sync --rebuild                    # all plugins
        ///   rvpm sync --rebuild nvim-treesitter    # only matching ones
        #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "QUERY")]
        rebuild: Option<String>,
        /// Force-refresh every plugin's git state regardless of the fetch
        /// cache (`options.fetch_interval`). Useful before checking for
        /// held-back plugins when you want a guaranteed fresh remote read.
        #[arg(long, conflicts_with = "no_refresh")]
        refresh: bool,
        /// Skip git fetch for every plugin regardless of the fetch cache
        /// (offline mode). Plugins whose local HEAD already matches the
        /// effective rev complete instantly; others fall through to a local
        /// checkout that errors if the commit isn't already available.
        #[arg(long, conflicts_with = "refresh")]
        no_refresh: bool,
    },

    /// Regenerate loader.lua only (no git)
    ///
    /// Useful after editing per-plugin init/before/after.lua or tweaking
    /// TOML triggers — skips the clone/pull phase entirely.
    ///
    /// Unchanged plugins (same clone HEAD + merge mode as the last run) keep
    /// their views/merged as-is via on-disk stamps, so repeat runs are fast.
    Generate {
        /// Rebuild every view and merged/ from scratch, ignoring the
        /// incremental stamps (use after hand-editing clones or build
        /// artifacts changed without a new commit).
        #[arg(long)]
        force: bool,
    },

    /// Delete plugin directories no longer referenced by config.toml
    ///
    /// Walks `{cache_root}/plugins/repos/` and removes every clone whose
    /// plugin is no longer in `config.toml`. Does not run git operations,
    /// so it is much faster than `sync --prune` on large configs
    /// (hundreds of plugins).
    Clean,

    /// Add a plugin and sync
    ///
    /// Accepts the same trigger flags as `set` to configure the plugin
    /// in one shot: `rvpm add owner/repo --on-cmd Foo`
    Add {
        /// Plugin repo: owner/repo, URL, or local path
        repo: String,

        /// Friendly name (optional)
        #[arg(long)]
        name: Option<String>,

        /// Set lazy flag
        #[arg(long)]
        lazy: Option<bool>,

        /// Set on_cmd. Comma-separated or JSON array.
        #[arg(long)]
        on_cmd: Option<String>,

        /// Set on_ft. Comma-separated or JSON array.
        #[arg(long)]
        on_ft: Option<String>,

        /// Set on_map. Comma-separated or JSON array/object.
        #[arg(long)]
        on_map: Option<String>,

        /// Set on_event. Comma-separated or JSON array.
        #[arg(long)]
        on_event: Option<String>,

        /// Set rev (branch/tag/commit)
        #[arg(long)]
        rev: Option<String>,

        /// Set setup, as a TOML inline table. rvpm then calls the plugin's
        /// `setup()` for you: `--setup '{}'` calls it with no options,
        /// `--setup '{ notify = true }'` passes that table, and
        /// `--setup '{ main = "mini.pick", opts = {} }'` also names the module
        /// to require. Data only — options needing Lua functions belong in
        /// after.lua.
        #[arg(long)]
        setup: Option<String>,

        /// Accept auto-scanned on_cmd / on_map without prompting.
        /// Overrides `options.auto_lazy` for this call (== "always").
        /// Useful for non-TTY scripts that want lazy-by-default.
        #[arg(long, conflicts_with = "no_lazy")]
        auto_lazy: bool,

        /// Skip the auto-scan entirely for this invocation.
        /// Overrides `options.auto_lazy` for this call (== "never").
        /// This does not override explicit `--lazy` / `--on-*` flags —
        /// if the plugin is lazy via those, it stays lazy.
        #[arg(long)]
        no_lazy: bool,

        /// AI backend for this `add`. Replaces the static-scan + auto-lazy path:
        /// the chosen CLI (`claude` / `agy` / `codex` / `opencode`) reads the
        /// plugin's README + your config and proposes the full `[[plugins]]`
        /// block plus any per-plugin hook files. Overrides `options.ai` for this
        /// call. `gemini` is deprecated — prefer `agy`, its successor.
        #[arg(long, value_enum, conflicts_with = "no_ai")]
        ai: Option<crate::config::AiBackend>,

        /// Force the static-scan path even if `options.ai` is set in config.
        #[arg(long)]
        no_ai: bool,
    },

    /// Tune an existing plugin's config with the AI backend
    ///
    /// Like `add --ai`, but for plugins **already configured** in
    /// `config.toml`. Picks one entry (by `[query]` fuzzy match or
    /// interactive select), feeds the current `[[plugins]]` block plus
    /// the cloned plugin's README/doc and any existing per-plugin hook
    /// files to the AI CLI, and asks for two parallel proposals per
    /// section: a fresh redesign and a merged variant that preserves
    /// your edits.
    ///
    /// At Apply time you pick per-section: `Use FRESH` (overwrite),
    /// `Use MERGED` (overwrite, keep your edits), or `Keep existing`
    /// (no change). Same choice applies to the `[[plugins]]` entry
    /// itself.
    ///
    /// Note: selecting `FRESH` or `MERGED` still overwrites that section.
    /// For the `[[plugins]]` entry, fields the chosen proposal omits are
    /// removed (destructive replace); only `Keep existing` leaves the
    /// current entry untouched. Use the Chat action to tell the AI to
    /// keep a particular field, or just pick `Keep existing` for that
    /// section.
    Tune {
        /// Fuzzy match plugin url (omit to pick interactively)
        query: Option<String>,

        /// AI backend for this `tune`. Overrides `options.ai` for this call.
        /// `gemini` is deprecated — prefer `agy`, its successor.
        #[arg(long, value_enum, conflicts_with = "no_ai")]
        ai: Option<crate::config::AiBackend>,

        /// Force-disable AI for this call. `tune` is AI-only, so this
        /// effectively errors out — provided for symmetry with `add`.
        #[arg(long)]
        no_ai: bool,
    },

    /// Edit per-plugin or global hook files in $EDITOR
    ///
    /// Without flags, prompts which plugin and file to edit.
    /// With --init / --before / --after, opens that file directly.
    /// With --global, edits global hooks instead of per-plugin files:
    ///   --init   = Neovim's own init.lua (`~/.config/<appname>/init.lua`)
    ///   --before = `<config_root>/before.lua`
    ///   --after  = `<config_root>/after.lua`
    Edit {
        /// Fuzzy match plugin url (omit to pick interactively)
        query: Option<String>,

        /// Open init.lua directly (per-plugin path, or Neovim's own with --global)
        #[arg(long)]
        init: bool,

        /// Open before.lua directly
        #[arg(long)]
        before: bool,

        /// Open after.lua directly
        #[arg(long)]
        after: bool,

        /// Edit global hooks instead of per-plugin files
        #[arg(long)]
        global: bool,
    },

    /// Tweak a plugin's options interactively
    ///
    /// Walks through lazy / merge / on_* / rev with fuzzy-select and
    /// ESC-cancellable prompts. Pick `[ Open config.toml in $EDITOR ]`
    /// to drop into raw TOML editing when you need table-form on_map
    /// or complex `cond` expressions.
    Set {
        /// Fuzzy match plugin url (omit to pick interactively)
        query: Option<String>,

        /// Set lazy flag non-interactively
        #[arg(long)]
        lazy: Option<bool>,

        /// Set merge flag non-interactively
        #[arg(long)]
        merge: Option<bool>,

        /// Set on_cmd. Comma-separated (`"Foo,Bar"`) or JSON array
        /// (`'["Foo","Bar"]'`).
        #[arg(long)]
        on_cmd: Option<String>,

        /// Set on_ft. Comma-separated or JSON array.
        #[arg(long)]
        on_ft: Option<String>,

        /// Set on_map. Comma-separated lhs list, JSON array of
        /// strings, or JSON array/object with full `{ lhs, mode, desc }`
        /// form. Example: --on-map '{"lhs":"<space>d","mode":["n","x"]}'
        #[arg(long)]
        on_map: Option<String>,

        /// Set on_event. Comma-separated or JSON array. Supports the
        /// `"User Xxx"` shorthand for User events with patterns.
        #[arg(long)]
        on_event: Option<String>,

        /// Set on_path glob list. Comma-separated or JSON array.
        #[arg(long)]
        on_path: Option<String>,

        /// Set on_source (plugin names). Comma-separated or JSON array.
        #[arg(long)]
        on_source: Option<String>,

        /// Set rev (branch/tag/commit) non-interactively
        #[arg(long)]
        rev: Option<String>,
    },

    /// Update (git pull) installed plugins
    Update {
        /// Fuzzy match plugin url (omit to update all)
        query: Option<String>,
        /// Bypass the supply-chain cooldown for this run and update straight
        /// to the remote tip (see `options.cooldown`; use for e.g. a security
        /// hotfix you want immediately). Note: this run records no
        /// observations, so habitually passing it keeps tips from maturing and
        /// later non-bypassed updates will hold back again
        #[arg(long)]
        no_cooldown: bool,
    },

    /// Remove a plugin and delete its directory
    Remove {
        /// Fuzzy match plugin url (omit to pick interactively)
        query: Option<String>,
    },

    /// Show plugin list (TUI by default, plain text with --no-tui)
    ///
    /// TUI keys: [q] quit  [j/k] move  [e] edit  [s] set  [S] sync all
    /// [u] update selected  [U] update all  [g] regenerate  [d] remove{n}
    /// With --no-tui: prints a sorted plain-text status line per plugin
    /// (pipe-friendly for scripting).
    List {
        /// Print plain text instead of launching the TUI
        #[arg(long)]
        no_tui: bool,
    },

    /// Open config.toml in $EDITOR
    ///
    /// Runs `sync` automatically after the editor exits.
    Config,

    /// Print or write the init.lua loader snippet
    ///
    /// Without --write: prints the exact `dofile(vim.fn.expand("..."))`
    /// line for your current config. Copy it into your Neovim init.lua.
    ///
    /// With --write: appends the snippet to `$NVIM_APPNAME`'s init.lua
    /// (defaults to `~/.config/nvim/init.lua`). If init.lua does not
    /// exist it is created with a header comment. Idempotent — a no-op
    /// if the loader is already referenced.
    Init {
        /// Append to init.lua (creates the file if missing)
        #[arg(long)]
        write: bool,
    },
    /// Browse and install Neovim plugins from GitHub
    Browse,

    /// Diagnose rvpm's config, state, and environment
    ///
    /// Inspects config.toml, the plugin cache, generated loader.lua, Neovim
    /// init.lua wiring, and required external tools (nvim / git / chezmoi /
    /// $EDITOR). Exits 0 on all-ok, 1 on any error, 2 on warn-only.
    Doctor,

    /// Profile Neovim startup time per plugin
    ///
    /// Spawns `nvim --headless --startuptime <tmp> +qa` N times, parses the
    /// output, and attributes each sourced file back to its plugin via path
    /// prefix match. By default emits phase markers into a temporarily
    /// instrumented loader.lua so that the TUI can show a per-phase
    /// breakdown (3=before / 4=init / 5=rtp / 6=eager / 7=lazy triggers /
    /// 9=after). The original loader.lua is restored on exit.
    /// Pipe-friendly plain text with `--no-tui`, JSON with `--json`.
    Profile {
        /// Number of nvim runs to average (default 3, max 20)
        #[arg(long, default_value_t = 3)]
        runs: usize,

        /// Limit plain / JSON output to top N plugins (TUI ignores this)
        #[arg(long)]
        top: Option<usize>,

        /// Emit the averaged report as JSON to stdout
        #[arg(long, conflicts_with = "no_tui")]
        json: bool,

        /// Plain text output instead of the TUI
        #[arg(long)]
        no_tui: bool,

        /// Treat all plugins as merge=false for this measurement, so each
        /// plugin's files source from their own repos/<canonical>/ path
        /// instead of the shared merged/ dir. Lets you see per-plugin load
        /// time even for plugins that are normally merged, and compare the
        /// cost of merging. merged/ itself is not touched.
        #[arg(long)]
        no_merge: bool,

        /// Skip phase-marker instrumentation. Faster and avoids swapping
        /// loader.lua during the profile run. You lose the phase timeline
        /// and per-plugin init/trig columns, but raw per-plugin self ms
        /// is still measured.
        #[arg(long)]
        no_instrument: bool,
    },

    /// Show recent sync/update/add changes
    ///
    /// Reads the per-run history persisted under `<cache_root>/update_log.json`
    /// and prints a human-readable digest of plugin commit changes.
    /// Pass a substring to filter by plugin name.
    Log {
        /// Case-insensitive substring filter on plugin display name
        query: Option<String>,

        /// How many recent runs to show (default: 1, max: 20)
        #[arg(long, default_value_t = crate::update_log::DEFAULT_LAST)]
        last: usize,

        /// Show full commit body in addition to subject (currently subject-only)
        #[arg(long)]
        full: bool,

        /// Inline `git diff` for changed README/CHANGELOG/doc files
        #[arg(long)]
        diff: bool,
    },

    /// Print a shell completion script to stdout (#114)
    ///
    /// Pipe the output into the appropriate location for your shell.
    /// Examples:
    ///
    ///   # bash (system-wide)
    ///   rvpm completion bash | sudo tee /etc/bash_completion.d/rvpm
    ///   # bash (user)
    ///   rvpm completion bash > ~/.local/share/bash-completion/completions/rvpm
    ///
    ///   # zsh — put it on $fpath, then `compinit`
    ///   rvpm completion zsh > ~/.zfunc/_rvpm
    ///
    ///   # fish
    ///   rvpm completion fish > ~/.config/fish/completions/rvpm.fish
    ///
    ///   # PowerShell — append to your $PROFILE
    ///   rvpm completion powershell >> $PROFILE
    Completion {
        /// Target shell (bash / zsh / fish / powershell / elvish)
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Update the rvpm binary itself to the latest GitHub release (#125)
    ///
    /// Detects how rvpm was installed (cargo install / direct binary / dev build)
    /// and dispatches accordingly. Exits without changes if already up to date.
    ///
    /// EXAMPLES:
    ///
    ///   # check + interactive prompt
    ///   rvpm self-update
    ///
    ///   # non-interactive (for scripts) — installs without asking
    ///   rvpm self-update --yes
    ///
    ///   # report availability and exit (no install)
    ///   rvpm self-update --check
    SelfUpdate {
        /// Skip the confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print availability and exit without installing
        #[arg(long)]
        check: bool,
    },
}

/// Library entry point invoked by the thin `rvpm` binary (`src/main.rs`).
///
/// Builds the multi-threaded Tokio runtime and dispatches the parsed CLI
/// command. The implementation lives in the library crate (rather than in
/// `main.rs`) so the entire command surface stays unit- and doc-testable and
/// can be embedded by other crates (#176).
pub fn run() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build Tokio runtime")?
        .block_on(run_cli())
}

async fn run_cli() -> Result<()> {
    let cli = Cli::parse();

    // 自動 update check は最初にバックグラウンド spawn して、 コマンド実行と並列に
    // GitHub API を fetch する (#125)。 結果は main 末尾で受け取って banner 出力。
    // SelfUpdate / Completion 時はノイズになるので spawn しない。
    let auto_update_handle = match cli.command {
        Some(Commands::SelfUpdate { .. } | Commands::Completion { .. }) => None,
        _ => maybe_spawn_auto_update_check().await,
    };

    match cli.command.unwrap_or(Commands::List { no_tui: false }) {
        Commands::Sync {
            prune,
            frozen,
            no_lock,
            rebuild,
            refresh,
            no_refresh,
        } => {
            run_sync(prune, frozen, no_lock, rebuild, refresh, no_refresh).await?;
        }
        Commands::Generate { force } => {
            run_generate(force).await?;
        }
        Commands::Clean => {
            run_clean()?;
        }
        Commands::Add {
            repo,
            name,
            lazy,
            on_cmd,
            on_ft,
            on_map,
            on_event,
            rev,
            setup,
            auto_lazy,
            no_lazy,
            ai,
            no_ai,
        } => {
            let policy_override = if auto_lazy {
                Some(crate::config::AutoLazyPolicy::Always)
            } else if no_lazy {
                Some(crate::config::AutoLazyPolicy::Never)
            } else {
                None
            };
            // `--no-ai` は config の `ai` を打ち消して Off に固定。`--ai <backend>` は
            // 明示指定 (Off 含む)。両方無指定 (None) なら config 値を使う。
            let ai_override = if no_ai {
                Some(crate::config::AiBackend::Off)
            } else {
                ai
            };
            run_add(
                repo,
                name,
                lazy,
                on_cmd,
                on_ft,
                on_map,
                on_event,
                rev,
                setup,
                policy_override,
                ai_override,
            )
            .await?;
        }
        Commands::Tune { query, ai, no_ai } => {
            // `--no-ai` は config の `ai` を打ち消して Off に固定。`--ai <backend>` は
            // 明示指定。両方無指定 (None) なら config 値を使う。
            // tune は AI 専用なので Off に解決した場合は run_tune 内で error する。
            let ai_override = if no_ai {
                Some(crate::config::AiBackend::Off)
            } else {
                ai
            };
            run_tune(query, ai_override).await?;
        }
        Commands::Edit {
            query,
            init,
            before,
            after,
            global,
        } => {
            if run_edit(query, init, before, after, global).await? {
                run_generate(false).await?;
            }
        }
        Commands::Set {
            query,
            lazy,
            merge,
            on_cmd,
            on_ft,
            on_map,
            on_event,
            on_path,
            on_source,
            rev,
        } => {
            if run_set(
                query, lazy, merge, on_cmd, on_ft, on_map, on_event, on_path, on_source, rev,
            )
            .await?
            {
                run_generate(false).await?;
            }
        }
        Commands::Update { query, no_cooldown } => {
            run_update(query, no_cooldown).await?;
        }
        Commands::Remove { query } => {
            run_remove(query).await?;
        }
        Commands::List { no_tui } => {
            // list / browse 間を相互に `b` / `l` キーで行き来できるように、
            // フラグが立っている限りループで切り替える。`--no-tui` は常に false を返すので即抜ける。
            let mut nt = no_tui;
            loop {
                if run_list(nt).await? && run_browse().await? {
                    nt = false;
                    continue;
                }
                break;
            }
        }
        Commands::Config => {
            if run_config().await? {
                run_generate(false).await?;
            }
        }
        Commands::Init { write } => {
            run_init(write).await?;
        }
        Commands::Browse => loop {
            if run_browse().await? && run_list(false).await? {
                continue;
            }
            break;
        },
        Commands::Doctor => {
            let code = run_doctor().await?;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Commands::Log {
            query,
            last,
            full,
            diff,
        } => {
            run_log(query, last, full, diff).await?;
        }
        Commands::Profile {
            runs,
            top,
            json,
            no_tui,
            no_merge,
            no_instrument,
        } => {
            run_profile(runs, top, json, no_tui, no_merge, no_instrument).await?;
        }
        Commands::Completion { shell } => {
            run_completion(shell);
        }
        Commands::SelfUpdate { yes, check } => {
            run_self_update(yes, check).await?;
        }
    }

    // バックグラウンドの auto update check を join + banner 出力 (#125)。
    // 既にコマンドの本体が完了してるので、 fetch がまだ動いてるなら短い timeout で
    // 切り上げる (1 秒) — banner は「あったら表示する」のスタンス。
    if let Some(handle) = auto_update_handle {
        finalize_auto_update_check(handle).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completion_generation_succeeds_for_all_shells() {
        // `rvpm completion <SHELL>` が clap_complete に渡る CLI 定義を毎回 panic
        // 無く読めることを確認 (#114)。 出力内容自体は clap_complete の責務なので
        // 中身は assert しないが、 何かしら出力されることだけは確認する。
        use clap::CommandFactory;
        use clap_complete::Shell;

        let shells = [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ];
        for shell in shells {
            let mut cmd = Cli::command();
            let bin = cmd.get_name().to_string();
            let mut out: Vec<u8> = Vec::new();
            clap_complete::generate(shell, &mut cmd, bin, &mut out);
            assert!(
                !out.is_empty(),
                "completion output for {:?} should not be empty",
                shell
            );
        }
    }
}
