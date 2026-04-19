//! `rvpm doctor` — 診断レポート生成モジュール。
//!
//! 設計指針:
//! - 各チェックは純粋関数 (読み取り FS I/O のみ許容) として実装し、テスト容易性を担保する。
//! - 1 つのチェック失敗が他のチェックを止めないように、呼び出し側で個別に catch する。
//!   (`run_checks` は個別の `try_*` 関数を順次呼ぶだけ。)
//! - 外部コマンドの実行は `VersionResolver` トレイト経由にし、ユニットテストでは
//!   モック実装を使う。
//!
//! 出力仕様は CLAUDE.md / issue #49 / caller の brief に従う。

use crate::config::{Config, IconStyle};
use crate::tui::Icons;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// 診断の深刻度。`Ok < Warn < Error` の順で重い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Ok,
    Warn,
    Error,
}

/// 1 つの診断項目。
///
/// - `category`: セクション見出し (`"Plugin config"`, `"State integrity"` 等)。
/// - `title`: 左端のラベル (`"depends cycles"` など)。固定幅でパディングされる。
/// - `summary`: `—` の後に出る短い要約 (`"none"` / `"94/94 resolved"` 等)。
/// - `details`: `└` / `├` で表示される詳細ライン。空なら詳細ブロックは出ない。
/// - `hint`: ワンライナーの復旧案内 (`hint: ...`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub category: String,
    pub title: String,
    pub summary: String,
    pub details: Vec<String>,
    pub hint: Option<String>,
}

impl Diagnostic {
    fn new(severity: Severity, category: &str, title: &str, summary: impl Into<String>) -> Self {
        Self {
            severity,
            category: category.to_string(),
            title: title.to_string(),
            summary: summary.into(),
            details: Vec::new(),
            hint: None,
        }
    }

    fn with_details(mut self, details: Vec<String>) -> Self {
        self.details = details;
        self
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

// ============================================================
// Categories (固定の並び順 + セクション見出し文字列)
// ============================================================

pub const CAT_PLUGIN_CONFIG: &str = "Plugin config";
pub const CAT_STATE_INTEGRITY: &str = "State integrity";
pub const CAT_NEOVIM: &str = "Neovim integration";
pub const CAT_TOOLS: &str = "External tools";

const CATEGORY_ORDER: &[&str] = &[
    CAT_PLUGIN_CONFIG,
    CAT_STATE_INTEGRITY,
    CAT_NEOVIM,
    CAT_TOOLS,
];

// ============================================================
// Plugin config: depends cycles
// ============================================================

/// `depends` に循環があるかを検出する。自分自身への `depends` も循環として扱う。
///
/// アルゴリズム: DFS ベースのサイクル検出。`sort_plugins` と同じ名前解決 (URL or
/// display_name) を使うが、resilience 原則のため `sort_plugins` 自体には副作用が
/// あるので、doctor は別途検査する。
pub fn check_depends_cycles(config: &Config) -> Diagnostic {
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for (i, p) in config.plugins.iter().enumerate() {
        by_key.insert(p.url.clone(), i);
        by_key.insert(p.display_name(), i);
    }

    let mut cycles: Vec<Vec<String>> = Vec::new();

    fn dfs(
        node: usize,
        plugins: &[crate::config::Plugin],
        by_key: &HashMap<String, usize>,
        visiting: &mut Vec<usize>,
        visited: &mut HashSet<usize>,
        cycles: &mut Vec<Vec<String>>,
    ) {
        if visited.contains(&node) {
            return;
        }
        if let Some(pos) = visiting.iter().position(|&n| n == node) {
            // 循環発見: visiting[pos..] が循環ノード
            let cycle: Vec<String> = visiting[pos..]
                .iter()
                .map(|&i| plugins[i].display_name())
                .chain(std::iter::once(plugins[node].display_name()))
                .collect();
            cycles.push(cycle);
            return;
        }
        visiting.push(node);
        if let Some(deps) = &plugins[node].depends {
            for dep in deps {
                if let Some(&next) = by_key.get(dep) {
                    dfs(next, plugins, by_key, visiting, visited, cycles);
                }
            }
        }
        visiting.pop();
        visited.insert(node);
    }

    let mut visited: HashSet<usize> = HashSet::new();
    for i in 0..config.plugins.len() {
        let mut visiting = Vec::new();
        dfs(
            i,
            &config.plugins,
            &by_key,
            &mut visiting,
            &mut visited,
            &mut cycles,
        );
    }

    // 重複除去: `[A, B, A]` と `[B, A, B]` は同じ循環。末尾の重複ノード (dfs が
    // `chain(once(node))` で付加している) を落としたノード集合を sort して正規形
    // にし、HashSet で dedupe する。末尾を落とさずに sort すると
    // `[A, A, B]` vs `[A, B, B]` で異なるキーになり dedupe 失敗する (gemini-code-assist
    // の指摘通り)。
    let mut normalized: HashSet<Vec<String>> = HashSet::new();
    let mut uniq_cycles: Vec<Vec<String>> = Vec::new();
    for cycle in cycles {
        let mut nodes = cycle.clone();
        if nodes.len() > 1 {
            nodes.pop();
        }
        nodes.sort();
        if normalized.insert(nodes) {
            uniq_cycles.push(cycle);
        }
    }

    if uniq_cycles.is_empty() {
        Diagnostic::new(Severity::Ok, CAT_PLUGIN_CONFIG, "depends cycles", "none")
    } else {
        let details: Vec<String> = uniq_cycles.iter().map(|c| c.join(" -> ")).collect();
        let summary = format!("{} found", uniq_cycles.len());
        Diagnostic::new(
            Severity::Error,
            CAT_PLUGIN_CONFIG,
            "depends cycles",
            summary,
        )
        .with_details(details)
        .with_hint("break the cycle in `depends`")
    }
}

/// `depends` の参照が他のプラグインに解決できるかを検査する。
pub fn check_depends_references(config: &Config) -> Diagnostic {
    let mut by_key: HashSet<String> = HashSet::new();
    for p in &config.plugins {
        by_key.insert(p.url.clone());
        by_key.insert(p.display_name());
    }

    let mut unresolved: Vec<String> = Vec::new();
    let mut total = 0usize;
    for p in &config.plugins {
        if let Some(deps) = &p.depends {
            for dep in deps {
                total += 1;
                if !by_key.contains(dep) {
                    unresolved.push(format!("{}: depends = [\"{}\"]", p.display_name(), dep));
                }
            }
        }
    }

    if unresolved.is_empty() {
        let summary = if total == 0 {
            "0/0 resolved".to_string()
        } else {
            format!("{}/{} resolved", total, total)
        };
        Diagnostic::new(
            Severity::Ok,
            CAT_PLUGIN_CONFIG,
            "depends references",
            summary,
        )
    } else {
        let resolved = total - unresolved.len();
        let summary = format!(
            "{}/{} resolved, {} missing",
            resolved,
            total,
            unresolved.len()
        );
        Diagnostic::new(
            Severity::Error,
            CAT_PLUGIN_CONFIG,
            "depends references",
            summary,
        )
        .with_details(unresolved)
        .with_hint("fix typos in `depends` or add the missing plugin")
    }
}

/// `on_source` の参照が他のプラグインに解決できるかを検査する。
/// 解決できない場合は "did you mean" サジェストを Levenshtein 距離で出す。
pub fn check_on_source_typos(config: &Config) -> Diagnostic {
    let names: Vec<String> = config.plugins.iter().map(|p| p.display_name()).collect();
    let mut all_keys: HashSet<String> = HashSet::new();
    for p in &config.plugins {
        all_keys.insert(p.url.clone());
        all_keys.insert(p.display_name());
    }

    let mut typos: Vec<String> = Vec::new();
    for p in &config.plugins {
        if let Some(sources) = &p.on_source {
            for src in sources {
                if !all_keys.contains(src) {
                    let suggestion = closest_name(src, &names);
                    let line = match suggestion {
                        Some(s) => format!(
                            "{}: on_source = [\"{}\"]  (hint: \"{}\"?)",
                            p.display_name(),
                            src,
                            s
                        ),
                        None => format!("{}: on_source = [\"{}\"]", p.display_name(), src),
                    };
                    typos.push(line);
                }
            }
        }
    }

    if typos.is_empty() {
        Diagnostic::new(Severity::Ok, CAT_PLUGIN_CONFIG, "on_source typos", "none")
    } else {
        let summary = format!("{} found", typos.len());
        Diagnostic::new(
            Severity::Warn,
            CAT_PLUGIN_CONFIG,
            "on_source typos",
            summary,
        )
        .with_details(typos)
    }
}

/// `dev = true` プラグインの `dst` が実在するかを検査する。
pub fn check_dev_plugin_dst(config: &Config) -> Diagnostic {
    let devs: Vec<&crate::config::Plugin> = config.plugins.iter().filter(|p| p.dev).collect();
    if devs.is_empty() {
        return Diagnostic::new(
            Severity::Ok,
            CAT_PLUGIN_CONFIG,
            "dev plugin dst",
            "0/0 exist",
        );
    }

    let mut missing: Vec<String> = Vec::new();
    for p in &devs {
        let dst = match &p.dst {
            Some(d) => crate::expand_tilde_public(d),
            None => {
                missing.push(format!("{}: no `dst` set", p.display_name()));
                continue;
            }
        };
        if !dst.exists() {
            missing.push(format!("{}: {}", p.display_name(), dst.display()));
        }
    }

    let total = devs.len();
    let ok = total - missing.len();
    if missing.is_empty() {
        Diagnostic::new(
            Severity::Ok,
            CAT_PLUGIN_CONFIG,
            "dev plugin dst",
            format!("{}/{} exist", ok, total),
        )
    } else {
        Diagnostic::new(
            Severity::Error,
            CAT_PLUGIN_CONFIG,
            "dev plugin dst",
            format!("{}/{} exist", ok, total),
        )
        .with_details(missing)
        .with_hint("set `dst` on the plugin or clone it locally")
    }
}

/// `url` / `name` の重複検出。url 重複のほうが重大なのでエラー、name 重複は警告。
pub fn check_duplicates(config: &Config) -> Diagnostic {
    let mut url_seen: HashMap<String, usize> = HashMap::new();
    let mut name_seen: HashMap<String, usize> = HashMap::new();
    for p in &config.plugins {
        *url_seen.entry(p.url.to_lowercase()).or_default() += 1;
        *name_seen.entry(p.display_name()).or_default() += 1;
    }

    let dup_urls: Vec<String> = url_seen
        .iter()
        .filter(|&(_, &c)| c > 1)
        .map(|(k, c)| format!("url \"{}\" appears {} times", k, c))
        .collect();
    let dup_names: Vec<String> = name_seen
        .iter()
        .filter(|&(_, &c)| c > 1)
        .map(|(k, c)| format!("name \"{}\" appears {} times", k, c))
        .collect();

    let mut details: Vec<String> = Vec::new();
    details.extend(dup_urls.iter().cloned());
    details.extend(dup_names.iter().cloned());
    details.sort();

    if details.is_empty() {
        Diagnostic::new(Severity::Ok, CAT_PLUGIN_CONFIG, "duplicates", "none")
    } else if !dup_urls.is_empty() {
        let summary = format!("{} found", details.len());
        Diagnostic::new(Severity::Error, CAT_PLUGIN_CONFIG, "duplicates", summary)
            .with_details(details)
            .with_hint("remove duplicated plugin entries in config.toml")
    } else {
        let summary = format!("{} found", details.len());
        Diagnostic::new(Severity::Warn, CAT_PLUGIN_CONFIG, "duplicates", summary)
            .with_details(details)
            .with_hint("rename one of the duplicated `name` fields")
    }
}

// ============================================================
// State integrity
// ============================================================

/// `config.toml` のプラグインが cache に clone されているかを検査する。
pub fn check_cloned_plugins<F>(config: &Config, resolve_dst: F) -> Diagnostic
where
    F: Fn(&crate::config::Plugin) -> PathBuf,
{
    let total = config.plugins.len();
    if total == 0 {
        return Diagnostic::new(Severity::Ok, CAT_STATE_INTEGRITY, "cloned plugins", "0/0");
    }

    let mut missing: Vec<String> = Vec::new();
    for p in &config.plugins {
        if p.dev {
            // dev は別チェック (check_dev_plugin_dst) で扱うのでスキップ。
            continue;
        }
        let dst = resolve_dst(p);
        if !dst.exists() {
            missing.push(format!("{}: {}", p.display_name(), dst.display()));
        }
    }

    let target_total = config.plugins.iter().filter(|p| !p.dev).count();
    let cloned = target_total - missing.len();
    if missing.is_empty() {
        Diagnostic::new(
            Severity::Ok,
            CAT_STATE_INTEGRITY,
            "cloned plugins",
            format!("{}/{}", cloned, target_total),
        )
    } else {
        Diagnostic::new(
            Severity::Warn,
            CAT_STATE_INTEGRITY,
            "cloned plugins",
            format!("{}/{}", cloned, target_total),
        )
        .with_details(missing)
        .with_hint("run `rvpm sync` to clone missing plugins")
    }
}

/// 未使用 repo ディレクトリの検出。リストは pre-sorted で渡される前提。
pub fn check_unused_cache_dirs(unused: &[PathBuf]) -> Diagnostic {
    if unused.is_empty() {
        return Diagnostic::new(
            Severity::Ok,
            CAT_STATE_INTEGRITY,
            "unused cache dirs",
            "none",
        );
    }
    let details: Vec<String> = unused.iter().map(|p| p.display().to_string()).collect();
    let summary = format!("{} found", unused.len());
    Diagnostic::new(
        Severity::Warn,
        CAT_STATE_INTEGRITY,
        "unused cache dirs",
        summary,
    )
    .with_details(details)
    .with_hint("`rvpm clean` or `rvpm sync --prune`")
}

/// merged/ 内の壊れた (存在しない target を指す) symlink を検出する。
/// 現行 link.rs はファイル単位 hard link なので symlink は基本作らないが、
/// 過去 (junction/symlink で merge していた時代) に作られた残骸や、ユーザーが
/// 手動で張ったリンクが壊れているケースを拾う。
pub fn check_merged_stale_links(merged_dir: &Path) -> Diagnostic {
    if !merged_dir.exists() {
        return Diagnostic::new(
            Severity::Ok,
            CAT_STATE_INTEGRITY,
            "merged/ stale links",
            "none",
        );
    }

    let mut stale: Vec<String> = Vec::new();
    // depth 制限なしで walk する。現行の link.rs は第 2 階層まで (`merged/<dir>/<file>`)
    // しか link を張らないが、plugin 同士の path 競合 (blink 系等) を避けるため
    // 将来的にファイル単位 link に移行する可能性がある。その場合 `merged/lua/foo/bar.lua`
    // のように深くなるので、今のうちから深い壊れ symlink も検出できるようにしておく。
    // `follow_links = false` でリンク自体を訪ねる (dead link の metadata が失敗する性質を利用)。
    for entry in walkdir::WalkDir::new(merged_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        // metadata は symlink の target が存在しないと Err。symlink_metadata で
        // symlink 自体があることを確認しつつ、metadata でその参照先を確認する。
        let symlink_meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if symlink_meta.file_type().is_symlink() && std::fs::metadata(path).is_err() {
            stale.push(path.display().to_string());
        }
    }

    if stale.is_empty() {
        Diagnostic::new(
            Severity::Ok,
            CAT_STATE_INTEGRITY,
            "merged/ stale links",
            "none",
        )
    } else {
        let summary = format!("{} found", stale.len());
        Diagnostic::new(
            Severity::Warn,
            CAT_STATE_INTEGRITY,
            "merged/ stale links",
            summary,
        )
        .with_details(stale)
        .with_hint("run `rvpm sync` to rebuild merged/")
    }
}

/// loader.lua が存在し、かつ config.toml より新しいかを検査する。
pub fn check_loader_freshness(loader_path: &Path, config_path: &Path) -> Diagnostic {
    let loader_mtime = match std::fs::metadata(loader_path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => {
            return Diagnostic::new(
                Severity::Error,
                CAT_STATE_INTEGRITY,
                "loader.lua freshness",
                "missing",
            )
            .with_hint("run `rvpm sync` or `rvpm generate`");
        }
    };

    let config_mtime = match std::fs::metadata(config_path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => {
            return Diagnostic::new(
                Severity::Warn,
                CAT_STATE_INTEGRITY,
                "loader.lua freshness",
                "config.toml unreadable",
            );
        }
    };

    if loader_mtime >= config_mtime {
        let age = format_age(loader_mtime);
        Diagnostic::new(
            Severity::Ok,
            CAT_STATE_INTEGRITY,
            "loader.lua freshness",
            format!("{} (newer than config.toml)", age),
        )
    } else {
        let age = format_age(loader_mtime);
        Diagnostic::new(
            Severity::Warn,
            CAT_STATE_INTEGRITY,
            "loader.lua freshness",
            format!("{} (older than config.toml)", age),
        )
        .with_hint("run `rvpm generate` to refresh loader.lua")
    }
}

fn format_age(mtime: std::time::SystemTime) -> String {
    match std::time::SystemTime::now().duration_since(mtime) {
        Ok(d) => {
            let secs = d.as_secs();
            if secs < 60 {
                format!("{}s ago", secs)
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86400)
            }
        }
        Err(_) => "future".to_string(),
    }
}

// ============================================================
// Neovim integration
// ============================================================

/// init.lua が loader.lua を dofile 経由で参照しているかを検査する。
pub fn check_init_lua_hook(init_lua_path: &Path) -> Diagnostic {
    if !init_lua_path.exists() {
        return Diagnostic::new(
            Severity::Warn,
            CAT_NEOVIM,
            "init.lua loader hook",
            "init.lua not found",
        )
        .with_hint("run `rvpm init --write`");
    }
    if crate::init_lua_references_rvpm_loader_public(init_lua_path) {
        Diagnostic::new(Severity::Ok, CAT_NEOVIM, "init.lua loader hook", "linked")
    } else {
        Diagnostic::new(
            Severity::Warn,
            CAT_NEOVIM,
            "init.lua loader hook",
            "no dofile(loader.lua)",
        )
        .with_hint("run `rvpm init --write`")
    }
}

/// appname coherence: 解決された appname と source env vars を表示する。
/// `$RVPM_APPNAME` と `$NVIM_APPNAME` の状態を注入可能にしてテスト容易に。
///
/// resolved appname と env var の値が食い違う場合は warn を返す。よくある罠:
/// - `RVPM_APPNAME=foo` だが値が path 区切りなどで invalid → fallback して
///   "nvim" になる (が、ユーザーは foo のつもり)
/// - `NVIM_APPNAME=bar` で nvim を起動しているのに `RVPM_APPNAME` が設定
///   されておらず rvpm 側は別の appname を解決している
pub fn check_appname(resolved: &str, rvpm_env: Option<&str>, nvim_env: Option<&str>) -> Diagnostic {
    let rvpm_state = match rvpm_env {
        None => "$RVPM_APPNAME unset".to_string(),
        Some(v) => format!("$RVPM_APPNAME={:?}", v),
    };
    let nvim_state = match nvim_env {
        None => "$NVIM_APPNAME unset".to_string(),
        Some(v) => format!("$NVIM_APPNAME={:?}", v),
    };
    let summary = format!("{:?}  ({}, {})", resolved, rvpm_state, nvim_state);

    // 不整合の検出 (どれか 1 つでも刺されば warn):
    let rvpm_mismatch = rvpm_env.is_some_and(|v| v != resolved);
    let nvim_mismatch = nvim_env.is_some_and(|v| v != resolved);
    let invalid_fallback = matches!(rvpm_env, Some(v) if v != resolved && resolved == "nvim")
        || matches!(nvim_env, Some(v) if v != resolved && resolved == "nvim");

    if rvpm_mismatch || nvim_mismatch {
        let mut details = Vec::new();
        if let Some(v) = rvpm_env
            && v != resolved
        {
            details.push(format!(
                "$RVPM_APPNAME={:?} but rvpm resolved {:?}",
                v, resolved
            ));
        }
        if let Some(v) = nvim_env
            && v != resolved
        {
            details.push(format!(
                "$NVIM_APPNAME={:?} but rvpm resolved {:?}",
                v, resolved
            ));
        }
        let hint = if invalid_fallback {
            "env var value rejected (path separators / `.` / `..` etc) — fell back to \"nvim\""
        } else {
            "env vars and rvpm's resolved appname disagree — config_root / cache_root may not match Neovim"
        };
        return Diagnostic::new(Severity::Warn, CAT_NEOVIM, "appname coherence", summary)
            .with_details(details)
            .with_hint(hint);
    }

    Diagnostic::new(Severity::Ok, CAT_NEOVIM, "appname coherence", summary)
}

/// `doc/` ディレクトリを `:helptags` の観点で分類した結果。
#[derive(Debug, PartialEq, Eq)]
pub enum DocStatus {
    /// `*.txt` または `*.??x` (Vim language-specific help) があり、対応する
    /// `tags` または `tags-<lang>` も存在する。
    HasTags,
    /// help ファイル (`*.txt` / `*.??x`) はあるが `tags` / `tags-*` が無い。
    /// `:helptags` の実行漏れ → warn 対象。
    MissingTags,
    /// `doc/` 内に help ファイルが 1 つも無い (画像のみ等)。
    /// `:helptags` の対象外なので OK 扱い。
    NoHelpFiles,
}

/// `doc/` ディレクトリを 1 段だけ走査し、help ファイルと tags ファイルの状況
/// から `DocStatus` を返す。Vim/Neovim の helptags は次の規則で動く:
///
/// - `<name>.txt` (英語ヘルプ) → `tags` ファイルが生成される
/// - `<name>.<lang>x` (例: `kensaku.jax`、language-specific help) →
///   `tags-<lang>` ファイルが生成される (e.g., `tags-ja`)
///
/// したがって `doc/tags` の有無だけで判定すると、日本語専用 plugin
/// (vim-kensaku 等) や画像のみの plugin (log-highlight.nvim 等) を誤検知する。
pub fn inspect_doc_dir(doc_dir: &Path) -> DocStatus {
    let entries = match std::fs::read_dir(doc_dir) {
        Ok(e) => e,
        Err(_) => return DocStatus::NoHelpFiles, // 読めなければ「help 無し」扱い
    };

    let mut has_help = false;
    let mut has_tags = false;
    for entry in entries.flatten() {
        // ディレクトリは判定対象外。`doc/tags-assets/` (画像置き場) や
        // `doc/manual.txt/` (誤って .txt 名のディレクトリが切られたケース) を
        // ファイルとカウントしない。
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }

        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let lower = name_str.to_ascii_lowercase();

        // help file 判定を tags 判定より先に。これは `tags-overview.txt` のような
        // 「先頭が tags- だが実は help file」を help 側に正しく分類するため。
        // <name>.txt
        if lower.ends_with(".txt") {
            has_help = true;
            continue;
        }
        // <name>.<lang>x — `<lang>` は 2-3 文字 (Vim 慣習) だが厳密にはチェック
        // しない: 拡張子が `x` で終わり、その手前が小文字英字 1+ なら language-
        // specific help とみなす (`.jax`, `.brx`, `.cnx` など)。
        if let Some((stem, ext)) = lower.rsplit_once('.')
            && !stem.is_empty()
            && ext.ends_with('x')
            && ext.len() >= 2
            && ext[..ext.len() - 1]
                .chars()
                .all(|c| c.is_ascii_alphabetic())
        {
            has_help = true;
            continue;
        }

        // tags / tags-ja / tags-en 等。Vim の tags ファイルは拡張子無しなので
        // `.bak`, `.old`, `.orig` 等をつけたバックアップは tags 扱いしない。
        if lower == "tags" || (lower.starts_with("tags-") && !lower.contains('.')) {
            has_tags = true;
        }
    }

    if !has_help {
        DocStatus::NoHelpFiles
    } else if has_tags {
        DocStatus::HasTags
    } else {
        DocStatus::MissingTags
    }
}

/// helptags チェック: `options.auto_helptags` に応じて挙動が変わる。
/// - false: informational (`Ok` で "disabled" と表示)
/// - true (default): `crate::helptags::collect_helptag_targets` と同じ規則で
///   実際に `:helptags` の対象になる `doc/` ディレクトリだけを列挙し、
///   各 target を `inspect_doc_dir` で分類する。
///
/// 判定ルール (per target):
/// - `HasTags` → 正常 (`tags` または `tags-<lang>` が存在)
/// - `NoHelpFiles` → `:helptags` の対象外なので skip (warn しない)
/// - `MissingTags` → warn 対象
///
/// 重要: eager + merge=true プラグインは `merged/doc/` に統合されるので、
/// 個別の plugin 配下の tags は生成されない (= ここでチェックしない)。
pub fn check_helptags(
    config: &Config,
    targets: &[PathBuf],
    target_labels: &[String],
) -> Diagnostic {
    if !config.options.auto_helptags {
        return Diagnostic::new(
            Severity::Ok,
            CAT_NEOVIM,
            "helptags",
            "disabled (options.auto_helptags = false)",
        );
    }

    let mut have_tags = 0usize;
    let mut considered = 0usize;
    let mut missing: Vec<String> = Vec::new();
    for (target, label) in targets.iter().zip(target_labels.iter()) {
        match inspect_doc_dir(target) {
            DocStatus::HasTags => {
                considered += 1;
                have_tags += 1;
            }
            DocStatus::MissingTags => {
                considered += 1;
                missing.push(format!("{}: {}", label, target.display()));
            }
            DocStatus::NoHelpFiles => {
                // `:helptags` の対象外。カウントから除外。
            }
        }
    }

    if missing.is_empty() {
        Diagnostic::new(
            Severity::Ok,
            CAT_NEOVIM,
            "helptags",
            format!("{}/{} have doc/tags", have_tags, considered),
        )
    } else {
        Diagnostic::new(
            Severity::Warn,
            CAT_NEOVIM,
            "helptags",
            format!("{}/{} have doc/tags", have_tags, considered),
        )
        .with_details(missing)
        .with_hint("run `rvpm sync` (auto_helptags) or `:helptags <doc>` in Neovim")
    }
}

// ============================================================
// External tools
// ============================================================

/// 外部コマンドのバージョンを取得する trait。テストでは mock 実装を差し込む。
///
/// `version()` は async — Tokio runtime 上で `tokio::process::Command` を spawn
/// し、`tokio::time::timeout` で stalled subprocess (壊れた PATH shim 等) を
/// 防ぐ。`env()` は同期で十分 (環境変数は in-process)。
#[async_trait::async_trait]
pub trait VersionResolver: Send + Sync {
    /// `cmd` (例: `"nvim"`, `"git"`) のバージョン文字列を返す。
    /// コマンドが無い / 失敗 / timeout した場合は None。
    async fn version(&self, cmd: &str) -> Option<String>;

    /// 環境変数の値 (None = unset)。
    fn env(&self, key: &str) -> Option<String>;
}

/// 外部コマンドの `--version` 実行に許す最大時間。これを越えたら諦めて None。
const VERSION_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// 本番実装: `tokio::process::Command` で `cmd --version` を実行して 1 行目を取得。
/// 2 秒のタイムアウト付き (壊れた PATH shim や stalled subprocess で hang しない)。
pub struct SystemResolver;

#[async_trait::async_trait]
impl VersionResolver for SystemResolver {
    async fn version(&self, cmd: &str) -> Option<String> {
        let fut = tokio::process::Command::new(cmd).arg("--version").output();
        let out = match tokio::time::timeout(VERSION_PROBE_TIMEOUT, fut).await {
            Ok(Ok(out)) => out,
            Ok(Err(_)) => return None, // spawn failed (cmd not found 等)
            Err(_) => return None,     // timeout
        };
        if !out.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout.lines().next()?.trim().to_string();
        if line.is_empty() { None } else { Some(line) }
    }

    fn env(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// version 文字列から `vX.Y.Z(-suffix)` っぽい部分を抜き出す。
/// 失敗時は全体を短縮して返す。
fn shorten_version(raw: &str) -> String {
    // `nvim --version` → "NVIM v0.13.0-dev-..."
    // `git --version` → "git version 2.49.1"
    // `chezmoi --version` → "chezmoi version v2.68.0, ..."
    // いずれも最初に `v?<digit>` が現れる場所をバージョンの先頭とみなし、
    // 続く非空白の範囲を取る。
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == 'v' && i + 1 < bytes.len() && (bytes[i + 1] as char).is_ascii_digit() {
            let token_end = raw[i..]
                .find(|ch: char| ch.is_whitespace() || ch == ',')
                .map(|p| i + p)
                .unwrap_or(raw.len());
            return raw[i..token_end].trim_end_matches(',').to_string();
        }
        if c.is_ascii_digit() {
            // version 番号 (prefix なし) — `v` を足して返す
            let token_end = raw[i..]
                .find(|ch: char| ch.is_whitespace() || ch == ',')
                .map(|p| i + p)
                .unwrap_or(raw.len());
            return format!("v{}", raw[i..token_end].trim_end_matches(','));
        }
        i += 1;
    }
    raw.to_string()
}

/// `nvim` コマンドの存在とバージョン。必須。
pub async fn check_tool_nvim(resolver: &dyn VersionResolver) -> Diagnostic {
    match resolver.version("nvim").await {
        Some(v) => Diagnostic::new(
            Severity::Ok,
            CAT_TOOLS,
            "nvim",
            format!("{}           (required)", shorten_version(&v)),
        ),
        None => Diagnostic::new(
            Severity::Error,
            CAT_TOOLS,
            "nvim",
            "not found            (required)",
        )
        .with_hint("install Neovim (https://neovim.io)"),
    }
}

/// `git` コマンドの存在とバージョン。必須。
pub async fn check_tool_git(resolver: &dyn VersionResolver) -> Diagnostic {
    match resolver.version("git").await {
        Some(v) => Diagnostic::new(
            Severity::Ok,
            CAT_TOOLS,
            "git",
            format!("{}            (required)", shorten_version(&v)),
        ),
        None => Diagnostic::new(
            Severity::Error,
            CAT_TOOLS,
            "git",
            "not found             (required)",
        )
        .with_hint("install git"),
    }
}

/// `chezmoi` コマンドの存在。`options.chezmoi = true` の時のみ必須、それ以外は
/// 情報表示。
pub async fn check_tool_chezmoi(config: &Config, resolver: &dyn VersionResolver) -> Diagnostic {
    let required = config.options.chezmoi;
    let label = if required {
        "(required: options.chezmoi=true)"
    } else {
        "(optional)"
    };
    match resolver.version("chezmoi").await {
        Some(v) => Diagnostic::new(
            Severity::Ok,
            CAT_TOOLS,
            "chezmoi",
            format!("{}            {}", shorten_version(&v), label),
        ),
        None => {
            let severity = if required {
                Severity::Error
            } else {
                Severity::Ok
            };
            Diagnostic::new(
                severity,
                CAT_TOOLS,
                "chezmoi",
                format!("not found            {}", label),
            )
        }
    }
}

/// `$EDITOR` 環境変数の状態。未設定なら警告。
pub fn check_editor(resolver: &dyn VersionResolver) -> Diagnostic {
    match resolver.env("EDITOR") {
        Some(e) if !e.trim().is_empty() => Diagnostic::new(
            Severity::Ok,
            CAT_TOOLS,
            "$EDITOR",
            format!("{}                  (required: for edit/config/set)", e),
        ),
        _ => Diagnostic::new(
            Severity::Warn,
            CAT_TOOLS,
            "$EDITOR",
            "unset                (required: for edit/config/set)",
        )
        .with_hint("set $EDITOR (e.g. `export EDITOR=nvim`)"),
    }
}

// ============================================================
// Helpers
// ============================================================

/// Levenshtein 距離。サイズ m × n の DP テーブルで素直に実装。
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut cur: Vec<usize> = vec![0; n + 1];
    for i in 1..=m {
        cur[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[n]
}

/// candidates の中から query に最も近い文字列を返す (距離 <= 3 のみ)。
/// 候補が無い / 閾値を超える場合は None。
fn closest_name(query: &str, candidates: &[String]) -> Option<String> {
    let mut best: Option<(&String, usize)> = None;
    for c in candidates {
        let d = levenshtein(query, c);
        match best {
            None => best = Some((c, d)),
            Some((_, bd)) if d < bd => best = Some((c, d)),
            _ => {}
        }
    }
    best.and_then(|(s, d)| if d <= 3 { Some(s.clone()) } else { None })
}

// ============================================================
// Orchestrator
// ============================================================

/// ユニットテストで `run_checks` を差し替え可能にするための入力集合。
pub struct CheckContext<'a> {
    pub config: &'a Config,
    pub config_path: &'a Path,
    pub loader_path: &'a Path,
    pub init_lua_path: &'a Path,
    pub merged_dir: &'a Path,
    pub unused_cache_dirs: Vec<PathBuf>,
    pub appname_resolved: String,
    pub rvpm_appname_env: Option<String>,
    pub nvim_appname_env: Option<String>,
    pub resolver: Box<dyn VersionResolver>,
    pub resolve_dst: Box<dyn Fn(&crate::config::Plugin) -> PathBuf + 'a>,
    /// `crate::helptags::collect_helptag_targets` で求めた `:helptags` 対象 doc/。
    /// merged/doc/ + lazy/non-merge プラグインの個別 doc/ のみが含まれる。
    pub helptag_targets: Vec<PathBuf>,
    /// `helptag_targets` と 1:1 で対応する表示用ラベル
    /// (例: "merged" / プラグイン名)。
    pub helptag_target_labels: Vec<String>,
}

pub async fn run_checks(ctx: &CheckContext<'_>) -> Vec<Diagnostic> {
    // 同期 check は即評価 — async に渡せない `&Fn` 参照を含むため。
    let sync_diags = vec![
        // Plugin config
        check_depends_cycles(ctx.config),
        check_depends_references(ctx.config),
        check_on_source_typos(ctx.config),
        check_dev_plugin_dst(ctx.config),
        check_duplicates(ctx.config),
        // State integrity
        check_cloned_plugins(ctx.config, |p| (ctx.resolve_dst)(p)),
        check_unused_cache_dirs(&ctx.unused_cache_dirs),
        check_merged_stale_links(ctx.merged_dir),
        check_loader_freshness(ctx.loader_path, ctx.config_path),
        // Neovim integration
        check_init_lua_hook(ctx.init_lua_path),
        check_appname(
            &ctx.appname_resolved,
            ctx.rvpm_appname_env.as_deref(),
            ctx.nvim_appname_env.as_deref(),
        ),
        check_helptags(ctx.config, &ctx.helptag_targets, &ctx.helptag_target_labels),
    ];

    // External tools は async — 並列に投げて待つことで合計レイテンシを下げる
    // (各 `cmd --version` が ~50ms 程度。順次なら 200ms、並列なら ~50ms)。
    let resolver = ctx.resolver.as_ref();
    let (nvim_d, git_d, chezmoi_d, editor_d) = tokio::join!(
        check_tool_nvim(resolver),
        check_tool_git(resolver),
        check_tool_chezmoi(ctx.config, resolver),
        async { check_editor(resolver) }, // editor は env() なので同期、形を揃えるだけ
    );

    let mut all = sync_diags;
    all.push(nvim_d);
    all.push(git_d);
    all.push(chezmoi_d);
    all.push(editor_d);
    all
}

// ============================================================
// Summary / exit code
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub ok: usize,
    pub warn: usize,
    pub error: usize,
}

impl Summary {
    pub fn from(diagnostics: &[Diagnostic]) -> Self {
        let mut s = Summary {
            ok: 0,
            warn: 0,
            error: 0,
        };
        for d in diagnostics {
            match d.severity {
                Severity::Ok => s.ok += 1,
                Severity::Warn => s.warn += 1,
                Severity::Error => s.error += 1,
            }
        }
        s
    }

    /// exit code: 1 if any error, 2 if any warn (no error), 0 otherwise.
    pub fn exit_code(&self) -> i32 {
        if self.error > 0 {
            1
        } else if self.warn > 0 {
            2
        } else {
            0
        }
    }
}

// ============================================================
// Rendering
// ============================================================

/// 左端のラベル (title) の固定幅。出力例の縦位置を揃える。
const TITLE_WIDTH: usize = 25;

/// 診断アイコンを Icons スタイルに合わせて返す (先頭プレフィクス)。
/// Ascii モードではタイトル列を揃えるため全 prefix を 4 文字に統一する
/// ("ok  " / "WARN" / "ERR ")。
fn severity_prefix(sev: Severity, icons: &Icons) -> String {
    match icons.style {
        IconStyle::Ascii => match sev {
            Severity::Ok => "ok  ".to_string(),
            Severity::Warn => "WARN".to_string(),
            Severity::Error => "ERR ".to_string(),
        },
        _ => match sev {
            Severity::Ok => "\u{2713}".to_string(),    // ✓
            Severity::Warn => "\u{26a0}".to_string(),  // ⚠
            Severity::Error => "\u{2717}".to_string(), // ✗
        },
    }
}

/// レンダリングで使う「文字種」の組。Ascii では box drawing / em-dash / middot
/// を全て ASCII 等価に落とす (CI ログ / 非 UTF-8 ターミナル向け)。
struct Glyphs {
    /// title と summary を区切る dash
    dash: &'static str,
    /// summary 句読点として使う中点
    middot: &'static str,
    /// details の最終要素 bullet
    last_bullet: &'static str,
    /// details の中間要素 bullet
    mid_bullet: &'static str,
}

fn glyphs_for(icons: &Icons) -> Glyphs {
    if matches!(icons.style, IconStyle::Ascii) {
        Glyphs {
            dash: "-",
            middot: ".",
            last_bullet: "`",
            mid_bullet: "|",
        }
    } else {
        Glyphs {
            dash: "\u{2014}",        // —
            middot: "\u{00b7}",      // ·
            last_bullet: "\u{2514}", // └
            mid_bullet: "\u{251c}",  // ├
        }
    }
}

pub fn render(diagnostics: &[Diagnostic], icons: &Icons) -> String {
    let g = glyphs_for(icons);
    let mut out = String::new();
    out.push_str(&format!("rvpm doctor {} diagnostic report\n\n", g.dash));

    for (ci, cat) in CATEGORY_ORDER.iter().enumerate() {
        let diags_in_cat: Vec<&Diagnostic> =
            diagnostics.iter().filter(|d| d.category == *cat).collect();
        if diags_in_cat.is_empty() {
            continue;
        }
        if ci > 0 {
            out.push('\n');
        }
        out.push_str(cat);
        out.push('\n');

        for d in diags_in_cat {
            let prefix = severity_prefix(d.severity, icons);
            let padded_title = pad_right(&d.title, TITLE_WIDTH);
            out.push_str(&format!(
                "  {} {} {} {}\n",
                prefix, padded_title, g.dash, d.summary
            ));

            // details: 最後の行は最終 bullet、それ以外は中間 bullet
            let n = d.details.len();
            for (i, line) in d.details.iter().enumerate() {
                let bullet = if i == n - 1 {
                    g.last_bullet
                } else {
                    g.mid_bullet
                };
                out.push_str(&format!("      {} {}\n", bullet, line));
            }
            if let Some(h) = &d.hint {
                out.push_str(&format!("      hint: {}\n", h));
            }
        }
    }

    let summary = Summary::from(diagnostics);
    out.push('\n');
    out.push_str(&format!(
        "Summary: {} ok  {}  {} warn  {}  {} error   (exit {})\n",
        summary.ok,
        g.middot,
        summary.warn,
        g.middot,
        summary.error,
        summary.exit_code()
    ));

    out
}

/// 文字列を最低 `width` 幅になるよう空白で右埋めする (UTF-8 safe: 文字数ベース)。
fn pad_right(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count >= width {
        s.to_string()
    } else {
        let mut out = String::with_capacity(s.len() + (width - count));
        out.push_str(s);
        for _ in count..width {
            out.push(' ');
        }
        out
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BrowseOptions, Config, Options, Plugin, UrlStyle};

    fn plugin(url: &str) -> Plugin {
        Plugin {
            url: url.to_string(),
            ..Default::default()
        }
    }

    fn mk_config(plugins: Vec<Plugin>) -> Config {
        Config {
            vars: None,
            options: Options {
                config_root: None,
                concurrency: None,
                cache_root: None,
                icons: IconStyle::Unicode,
                chezmoi: false,
                auto_clean: false,
                auto_helptags: false,
                url_style: UrlStyle::Short,
                browse: BrowseOptions::default(),
            },
            plugins,
        }
    }

    // -------- levenshtein --------

    #[test]
    fn test_levenshtein_basic() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("a", ""), 1);
        assert_eq!(levenshtein("", "ab"), 2);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("snack.nvim", "snacks.nvim"), 1);
    }

    // -------- depends cycles --------

    #[test]
    fn test_check_depends_cycles_none() {
        let cfg = mk_config(vec![
            Plugin {
                url: "A".into(),
                depends: Some(vec!["B".into()]),
                ..Default::default()
            },
            plugin("B"),
        ]);
        let d = check_depends_cycles(&cfg);
        assert_eq!(d.severity, Severity::Ok);
        assert_eq!(d.summary, "none");
    }

    #[test]
    fn test_check_depends_cycles_detects_loop() {
        let cfg = mk_config(vec![
            Plugin {
                url: "A".into(),
                depends: Some(vec!["B".into()]),
                ..Default::default()
            },
            Plugin {
                url: "B".into(),
                depends: Some(vec!["A".into()]),
                ..Default::default()
            },
        ]);
        let d = check_depends_cycles(&cfg);
        assert_eq!(d.severity, Severity::Error);
        assert!(!d.details.is_empty());
    }

    #[test]
    fn test_check_depends_cycles_dedupes_equivalent_cycles() {
        // A → B → A と B → A → B は同じ循環。dfs が両方検出しても dedupe で 1 件に。
        let cfg = mk_config(vec![
            Plugin {
                url: "A".into(),
                depends: Some(vec!["B".into()]),
                ..Default::default()
            },
            Plugin {
                url: "B".into(),
                depends: Some(vec!["A".into()]),
                ..Default::default()
            },
        ]);
        let d = check_depends_cycles(&cfg);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(
            d.details.len(),
            1,
            "equivalent cycles should dedupe, got: {:?}",
            d.details
        );
    }

    // -------- depends references --------

    #[test]
    fn test_check_depends_references_all_resolved() {
        let cfg = mk_config(vec![
            Plugin {
                url: "A".into(),
                depends: Some(vec!["B".into()]),
                ..Default::default()
            },
            plugin("B"),
        ]);
        let d = check_depends_references(&cfg);
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("1/1"));
    }

    #[test]
    fn test_check_depends_references_missing() {
        let cfg = mk_config(vec![Plugin {
            url: "A".into(),
            depends: Some(vec!["NOT_FOUND".into()]),
            ..Default::default()
        }]);
        let d = check_depends_references(&cfg);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.details.len(), 1);
    }

    // -------- on_source typos --------

    #[test]
    fn test_check_on_source_typos_suggests() {
        let cfg = mk_config(vec![
            Plugin {
                url: "owner/snacks.nvim".into(),
                ..Default::default()
            },
            Plugin {
                url: "nvim-telescope/telescope.nvim".into(),
                on_source: Some(vec!["snack.nvim".into()]),
                ..Default::default()
            },
        ]);
        let d = check_on_source_typos(&cfg);
        assert_eq!(d.severity, Severity::Warn);
        assert_eq!(d.details.len(), 1);
        assert!(d.details[0].contains("snacks.nvim"));
    }

    #[test]
    fn test_check_on_source_typos_exact_match_ok() {
        let cfg = mk_config(vec![
            Plugin {
                url: "owner/snacks.nvim".into(),
                ..Default::default()
            },
            Plugin {
                url: "nvim-telescope/telescope.nvim".into(),
                on_source: Some(vec!["snacks.nvim".into()]),
                ..Default::default()
            },
        ]);
        let d = check_on_source_typos(&cfg);
        assert_eq!(d.severity, Severity::Ok);
    }

    // -------- dev plugin dst --------

    #[test]
    fn test_check_dev_plugin_dst_missing() {
        let cfg = mk_config(vec![Plugin {
            url: "owner/devplugin".into(),
            dev: true,
            dst: Some("/this/path/should/never/exist/rvpm_test".into()),
            ..Default::default()
        }]);
        let d = check_dev_plugin_dst(&cfg);
        assert_eq!(d.severity, Severity::Error);
    }

    #[test]
    fn test_check_dev_plugin_dst_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = mk_config(vec![Plugin {
            url: "owner/devplugin".into(),
            dev: true,
            dst: Some(tmp.path().to_string_lossy().to_string()),
            ..Default::default()
        }]);
        let d = check_dev_plugin_dst(&cfg);
        assert_eq!(d.severity, Severity::Ok);
    }

    // -------- duplicates --------

    #[test]
    fn test_check_duplicates_url_error() {
        let cfg = mk_config(vec![plugin("owner/repo"), plugin("owner/repo")]);
        let d = check_duplicates(&cfg);
        assert_eq!(d.severity, Severity::Error);
    }

    #[test]
    fn test_check_duplicates_none() {
        let cfg = mk_config(vec![plugin("owner/a"), plugin("owner/b")]);
        let d = check_duplicates(&cfg);
        assert_eq!(d.severity, Severity::Ok);
    }

    // -------- cloned plugins --------

    #[test]
    fn test_check_cloned_plugins_all_present() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let p_path = root.join("owner-a");
        std::fs::create_dir_all(&p_path).unwrap();
        let cfg = mk_config(vec![plugin("owner/a")]);
        let r = root.clone();
        let d = check_cloned_plugins(&cfg, move |_p| r.join("owner-a"));
        assert_eq!(d.severity, Severity::Ok);
    }

    #[test]
    fn test_check_cloned_plugins_missing() {
        let cfg = mk_config(vec![plugin("owner/a")]);
        let d = check_cloned_plugins(&cfg, |_p| PathBuf::from("/nonexistent/rvpm_test"));
        assert_eq!(d.severity, Severity::Warn);
    }

    // -------- unused cache dirs --------

    #[test]
    fn test_check_unused_none() {
        let d = check_unused_cache_dirs(&[]);
        assert_eq!(d.severity, Severity::Ok);
    }

    #[test]
    fn test_check_unused_some() {
        let d = check_unused_cache_dirs(&[PathBuf::from("/x/y/z")]);
        assert_eq!(d.severity, Severity::Warn);
        assert_eq!(d.details.len(), 1);
    }

    // -------- loader freshness --------

    #[test]
    fn test_check_loader_freshness_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let loader = tmp.path().join("loader.lua");
        let config = tmp.path().join("config.toml");
        std::fs::write(&config, "x").unwrap();
        let d = check_loader_freshness(&loader, &config);
        assert_eq!(d.severity, Severity::Error);
    }

    #[test]
    fn test_check_loader_freshness_ok_when_newer() {
        let tmp = tempfile::tempdir().unwrap();
        let loader = tmp.path().join("loader.lua");
        let config = tmp.path().join("config.toml");
        std::fs::write(&config, "x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&loader, "y").unwrap();
        let d = check_loader_freshness(&loader, &config);
        assert_eq!(d.severity, Severity::Ok);
    }

    #[test]
    fn test_check_loader_freshness_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let loader = tmp.path().join("loader.lua");
        let config = tmp.path().join("config.toml");
        std::fs::write(&loader, "y").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&config, "x").unwrap();
        let d = check_loader_freshness(&loader, &config);
        assert_eq!(d.severity, Severity::Warn);
    }

    // -------- init.lua hook --------

    #[test]
    fn test_check_init_lua_hook_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        let d = check_init_lua_hook(&init);
        assert_eq!(d.severity, Severity::Warn);
    }

    #[test]
    fn test_check_init_lua_hook_linked() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(
            &init,
            "-- some comment\ndofile(vim.fn.expand(\"~/.cache/rvpm/loader.lua\"))\n",
        )
        .unwrap();
        let d = check_init_lua_hook(&init);
        assert_eq!(d.severity, Severity::Ok);
    }

    // -------- appname --------

    #[test]
    fn test_check_appname_reports_env_state() {
        let d = check_appname("nvim", None, None);
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("\"nvim\""));
        assert!(d.summary.contains("$RVPM_APPNAME unset"));
        assert!(d.summary.contains("$NVIM_APPNAME unset"));
    }

    #[test]
    fn test_check_appname_with_env_set_matching() {
        // env と resolved が一致 → ok
        let d = check_appname("mynvim", Some("mynvim"), None);
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("$RVPM_APPNAME=\"mynvim\""));
    }

    #[test]
    fn test_check_appname_warns_on_rvpm_mismatch() {
        // RVPM_APPNAME=foo だが resolved は別 → warn
        let d = check_appname("nvim", Some("foo"), None);
        assert_eq!(d.severity, Severity::Warn);
        assert!(
            d.details
                .iter()
                .any(|s| s.contains("$RVPM_APPNAME=\"foo\"") && s.contains("\"nvim\"")),
            "expected mismatch detail, got: {:?}",
            d.details
        );
    }

    #[test]
    fn test_check_appname_warns_on_nvim_mismatch() {
        // NVIM_APPNAME=bar で nvim を起動しているのに rvpm は別を解決 → warn
        let d = check_appname("nvim", None, Some("bar"));
        assert_eq!(d.severity, Severity::Warn);
        assert!(
            d.details
                .iter()
                .any(|s| s.contains("$NVIM_APPNAME=\"bar\""))
        );
    }

    #[test]
    fn test_check_appname_warns_on_invalid_fallback() {
        // 無効な値 (path 区切り含む等) で fallback して "nvim" になったケース
        // (ここでは resolved=nvim、env=invalid を擬似的に再現)
        let d = check_appname("nvim", Some("foo/bar"), None);
        assert_eq!(d.severity, Severity::Warn);
        assert!(
            d.hint.as_deref().is_some_and(|h| h.contains("rejected")),
            "expected fallback hint, got hint={:?}",
            d.hint
        );
    }

    #[test]
    fn test_check_appname_ok_when_both_env_match() {
        let d = check_appname("custom", Some("custom"), Some("custom"));
        assert_eq!(d.severity, Severity::Ok);
    }

    // -------- helptags --------

    #[test]
    fn test_check_helptags_disabled_when_opted_out() {
        // auto_helptags = false を明示すると check 自体をスキップして informational に。
        let mut cfg = mk_config(vec![plugin("owner/a")]);
        cfg.options.auto_helptags = false;
        let d = check_helptags(&cfg, &[], &[]);
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("disabled"));
    }

    #[test]
    fn test_check_helptags_warns_when_missing_tags() {
        // help ファイルがあるが tags 無し → warn
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("plugin/doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("foo.txt"), b"*foo* help text").unwrap();
        let mut cfg = mk_config(vec![plugin("owner/a")]);
        cfg.options.auto_helptags = true;
        let d = check_helptags(&cfg, &[doc], &["owner/a".into()]);
        assert_eq!(d.severity, Severity::Warn);
        assert!(d.summary.contains("0/1"));
    }

    #[test]
    fn test_check_helptags_ok_when_tags_present() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("plugin/doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("foo.txt"), b"*foo* help").unwrap();
        std::fs::write(doc.join("tags"), b"foo\tfoo.txt\t/*foo*\n").unwrap();
        let mut cfg = mk_config(vec![plugin("owner/a")]);
        cfg.options.auto_helptags = true;
        let d = check_helptags(&cfg, &[doc], &["owner/a".into()]);
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("1/1"));
    }

    #[test]
    fn test_check_helptags_ok_with_language_tags_only() {
        // 日本語専用 plugin (kensaku.jax + tags-ja) は tags が無くても OK
        // (vim-kensaku, vim-colorscheme-kemonofriends 等)。
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("plugin/doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("kensaku.jax"), b"*kensaku-ja* japanese help").unwrap();
        std::fs::write(
            doc.join("tags-ja"),
            b"kensaku-ja\tkensaku.jax\t/*kensaku-ja*\n",
        )
        .unwrap();
        let mut cfg = mk_config(vec![plugin("owner/a")]);
        cfg.options.auto_helptags = true;
        let d = check_helptags(&cfg, &[doc], &["owner/a".into()]);
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("1/1"));
    }

    #[test]
    fn test_check_helptags_skips_doc_with_no_help_files() {
        // doc/images/ のみ (log-highlight.nvim 等) は :helptags の対象外、
        // count から除外して 0/0 OK にする。
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("plugin/doc");
        std::fs::create_dir_all(doc.join("images")).unwrap();
        let mut cfg = mk_config(vec![plugin("owner/a")]);
        cfg.options.auto_helptags = true;
        let d = check_helptags(&cfg, &[doc], &["owner/a".into()]);
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("0/0"));
    }

    #[test]
    fn test_check_helptags_no_targets_means_zero_zero() {
        // sync 後に doc/ があるプラグインがゼロというケース。OK 表示。
        let mut cfg = mk_config(vec![plugin("owner/a")]);
        cfg.options.auto_helptags = true;
        let d = check_helptags(&cfg, &[], &[]);
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("0/0"));
    }

    // -------- inspect_doc_dir --------

    #[test]
    fn test_inspect_doc_dir_missing_returns_no_help() {
        let tmp = tempfile::tempdir().unwrap();
        let nope = tmp.path().join("does/not/exist");
        assert_eq!(inspect_doc_dir(&nope), DocStatus::NoHelpFiles);
    }

    #[test]
    fn test_inspect_doc_dir_empty_returns_no_help() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        assert_eq!(inspect_doc_dir(&doc), DocStatus::NoHelpFiles);
    }

    #[test]
    fn test_inspect_doc_dir_only_images_returns_no_help() {
        // doc/images/ サブディレクトリのみ — help なし。
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(doc.join("images")).unwrap();
        assert_eq!(inspect_doc_dir(&doc), DocStatus::NoHelpFiles);
    }

    #[test]
    fn test_inspect_doc_dir_txt_without_tags_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("plugin.txt"), b"*plugin*").unwrap();
        assert_eq!(inspect_doc_dir(&doc), DocStatus::MissingTags);
    }

    #[test]
    fn test_inspect_doc_dir_txt_with_tags_has_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("plugin.txt"), b"*plugin*").unwrap();
        std::fs::write(doc.join("tags"), b"plugin\tplugin.txt\t/*plugin*\n").unwrap();
        assert_eq!(inspect_doc_dir(&doc), DocStatus::HasTags);
    }

    #[test]
    fn test_inspect_doc_dir_jax_with_tags_ja_has_tags() {
        // kensaku.jax + tags-ja = 日本語ヘルプの正常状態
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("kensaku.jax"), b"*kensaku-ja*").unwrap();
        std::fs::write(doc.join("tags-ja"), b"kensaku-ja\tkensaku.jax\n").unwrap();
        assert_eq!(inspect_doc_dir(&doc), DocStatus::HasTags);
    }

    #[test]
    fn test_inspect_doc_dir_jax_without_tags_ja_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("kensaku.jax"), b"*kensaku-ja*").unwrap();
        assert_eq!(inspect_doc_dir(&doc), DocStatus::MissingTags);
    }

    #[test]
    fn test_inspect_doc_dir_tags_case_insensitive() {
        // 大文字 TAGS はあまり無いが念のため
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("plugin.txt"), b"*plugin*").unwrap();
        std::fs::write(doc.join("TAGS"), b"plugin\tplugin.txt\n").unwrap();
        assert_eq!(inspect_doc_dir(&doc), DocStatus::HasTags);
    }

    #[test]
    fn test_inspect_doc_dir_skips_directory_named_like_help_or_tags() {
        // ディレクトリは中身に関わらず判定対象外。`doc/manual.txt/` (誤って
        // .txt 名のディレクトリ) を has_help、`doc/tags-assets/` を has_tags
        // としてカウントしないこと。
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(doc.join("manual.txt")).unwrap();
        std::fs::create_dir_all(doc.join("tags-assets")).unwrap();
        // 実体の help / tags ファイルはどちらも無い
        assert_eq!(inspect_doc_dir(&doc), DocStatus::NoHelpFiles);
    }

    #[test]
    fn test_inspect_doc_dir_tags_with_extension_is_not_tags_file() {
        // tags-ja.bak / tags.old のようなバックアップは tags ファイルではない。
        // (Vim の tags ファイルは拡張子を持たない。)
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("plugin.txt"), b"*plugin*").unwrap();
        std::fs::write(doc.join("tags.bak"), b"old").unwrap();
        std::fs::write(doc.join("tags-ja.old"), b"old-ja").unwrap();
        // help はあるが tags は無い → MissingTags
        assert_eq!(inspect_doc_dir(&doc), DocStatus::MissingTags);
    }

    #[test]
    fn test_inspect_doc_dir_help_named_like_tags_counts_as_help() {
        // `tags-overview.txt` のような「先頭が tags- だが拡張子 .txt」は
        // help file として扱う (tags 検出より .txt 検出を先にしているため)。
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("tags-overview.txt"), b"*tags-overview*").unwrap();
        std::fs::write(doc.join("tags"), b"tags-overview\ttags-overview.txt\n").unwrap();
        assert_eq!(inspect_doc_dir(&doc), DocStatus::HasTags);
    }

    #[test]
    fn test_inspect_doc_dir_jax_named_like_tags_counts_as_help() {
        // `tags-overview.jax` のような「先頭が tags- だが拡張子 .jax」も
        // language-specific help として扱う。判定順序の bug guard。
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("tags-overview.jax"), b"*tags-ja*").unwrap();
        // tags-ja があるので OK
        std::fs::write(doc.join("tags-ja"), b"...").unwrap();
        assert_eq!(inspect_doc_dir(&doc), DocStatus::HasTags);
    }

    // -------- external tools --------

    struct MockResolver {
        map: HashMap<String, String>,
        env: HashMap<String, String>,
    }

    impl MockResolver {
        fn new() -> Self {
            Self {
                map: HashMap::new(),
                env: HashMap::new(),
            }
        }
        fn with_cmd(mut self, cmd: &str, ver: &str) -> Self {
            self.map.insert(cmd.to_string(), ver.to_string());
            self
        }
        fn with_env(mut self, key: &str, val: &str) -> Self {
            self.env.insert(key.to_string(), val.to_string());
            self
        }
    }

    #[async_trait::async_trait]
    impl VersionResolver for MockResolver {
        async fn version(&self, cmd: &str) -> Option<String> {
            self.map.get(cmd).cloned()
        }
        fn env(&self, key: &str) -> Option<String> {
            self.env.get(key).cloned()
        }
    }

    #[tokio::test]
    async fn test_check_tool_nvim_present() {
        let r = MockResolver::new().with_cmd("nvim", "NVIM v0.13.0-dev-some-build");
        let d = check_tool_nvim(&r).await;
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("v0.13.0-dev-some-build"));
    }

    #[tokio::test]
    async fn test_check_tool_nvim_missing() {
        let r = MockResolver::new();
        let d = check_tool_nvim(&r).await;
        assert_eq!(d.severity, Severity::Error);
    }

    #[tokio::test]
    async fn test_check_tool_git_present() {
        let r = MockResolver::new().with_cmd("git", "git version 2.49.1");
        let d = check_tool_git(&r).await;
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("v2.49.1"));
    }

    #[tokio::test]
    async fn test_check_tool_chezmoi_optional_when_disabled() {
        let cfg = mk_config(vec![]);
        let r = MockResolver::new();
        let d = check_tool_chezmoi(&cfg, &r).await;
        // chezmoi 無し + options.chezmoi = false → Ok (informational)
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("optional"));
    }

    #[tokio::test]
    async fn test_check_tool_chezmoi_required_when_enabled() {
        let mut cfg = mk_config(vec![]);
        cfg.options.chezmoi = true;
        let r = MockResolver::new();
        let d = check_tool_chezmoi(&cfg, &r).await;
        // chezmoi 無し + options.chezmoi = true → Error
        assert_eq!(d.severity, Severity::Error);
    }

    #[test]
    fn test_check_editor_set() {
        let r = MockResolver::new().with_env("EDITOR", "vim");
        let d = check_editor(&r);
        assert_eq!(d.severity, Severity::Ok);
        assert!(d.summary.contains("vim"));
    }

    #[test]
    fn test_check_editor_unset() {
        let r = MockResolver::new();
        let d = check_editor(&r);
        assert_eq!(d.severity, Severity::Warn);
    }

    #[tokio::test]
    async fn test_system_resolver_handles_missing_command() {
        // SystemResolver の version() はコマンド不在で None を返す (resilient)
        let r = SystemResolver;
        let d = r.version("__rvpm_nonexistent_cmd__").await;
        assert_eq!(d, None);
    }

    // -------- summary / exit code --------

    #[test]
    fn test_summary_counts() {
        let diags = vec![
            Diagnostic::new(Severity::Ok, CAT_PLUGIN_CONFIG, "a", "x"),
            Diagnostic::new(Severity::Ok, CAT_PLUGIN_CONFIG, "b", "x"),
            Diagnostic::new(Severity::Warn, CAT_PLUGIN_CONFIG, "c", "x"),
            Diagnostic::new(Severity::Error, CAT_PLUGIN_CONFIG, "d", "x"),
        ];
        let s = Summary::from(&diags);
        assert_eq!(s.ok, 2);
        assert_eq!(s.warn, 1);
        assert_eq!(s.error, 1);
        assert_eq!(s.exit_code(), 1);
    }

    #[test]
    fn test_exit_code_warn_only() {
        let diags = vec![
            Diagnostic::new(Severity::Ok, CAT_PLUGIN_CONFIG, "a", "x"),
            Diagnostic::new(Severity::Warn, CAT_PLUGIN_CONFIG, "b", "x"),
        ];
        let s = Summary::from(&diags);
        assert_eq!(s.exit_code(), 2);
    }

    #[test]
    fn test_exit_code_all_ok() {
        let diags = vec![Diagnostic::new(Severity::Ok, CAT_PLUGIN_CONFIG, "a", "x")];
        let s = Summary::from(&diags);
        assert_eq!(s.exit_code(), 0);
    }

    #[test]
    fn test_exit_code_error_outweighs_warn() {
        let diags = vec![
            Diagnostic::new(Severity::Warn, CAT_PLUGIN_CONFIG, "a", "x"),
            Diagnostic::new(Severity::Error, CAT_PLUGIN_CONFIG, "b", "x"),
        ];
        let s = Summary::from(&diags);
        assert_eq!(s.exit_code(), 1);
    }

    // -------- render --------

    #[test]
    fn test_render_basic_shape() {
        let diags = vec![
            Diagnostic::new(Severity::Ok, CAT_PLUGIN_CONFIG, "depends cycles", "none"),
            Diagnostic::new(
                Severity::Warn,
                CAT_PLUGIN_CONFIG,
                "on_source typos",
                "1 found",
            )
            .with_details(vec!["a: on_source = [\"b\"]  (hint: \"c\"?)".into()]),
            Diagnostic::new(Severity::Ok, CAT_NEOVIM, "init.lua loader hook", "linked"),
        ];
        let icons = Icons::from_style(IconStyle::Unicode);
        let out = render(&diags, &icons);
        assert!(out.contains("rvpm doctor"));
        assert!(out.contains("Plugin config"));
        assert!(out.contains("Neovim integration"));
        assert!(out.contains("\u{2713}")); // ✓
        assert!(out.contains("\u{26a0}")); // ⚠
        assert!(out.contains("└ a: on_source")); // last-only detail uses └
        assert!(out.contains("Summary:"));
        assert!(out.contains("exit 2"));
    }

    #[test]
    fn test_render_ascii_prefixes() {
        let diags = vec![
            Diagnostic::new(Severity::Ok, CAT_PLUGIN_CONFIG, "x", "done"),
            Diagnostic::new(Severity::Warn, CAT_PLUGIN_CONFIG, "y", "meh"),
            Diagnostic::new(Severity::Error, CAT_PLUGIN_CONFIG, "z", "bad"),
        ];
        let icons = Icons::from_style(IconStyle::Ascii);
        let out = render(&diags, &icons);
        assert!(out.contains("ok  "));
        assert!(out.contains("WARN"));
        assert!(out.contains("ERR "));
    }

    #[test]
    fn test_render_ascii_is_pure_ascii() {
        // ASCII モードでは出力が完全に ASCII で完結し、box drawing /
        // em-dash / middot / Nerd Font 文字を含まないこと。
        let diags = vec![
            Diagnostic::new(Severity::Ok, CAT_PLUGIN_CONFIG, "title-a", "done"),
            Diagnostic::new(Severity::Warn, CAT_STATE_INTEGRITY, "title-b", "1 found")
                .with_details(vec!["alpha".into(), "beta".into()])
                .with_hint("try `rvpm clean`"),
        ];
        let icons = Icons::from_style(IconStyle::Ascii);
        let out = render(&diags, &icons);
        for ch in out.chars() {
            assert!(
                ch.is_ascii(),
                "non-ASCII char {:?} (U+{:04X}) found in ASCII output:\n{}",
                ch,
                ch as u32,
                out
            );
        }
        // セパレータ系も置換されていること
        assert!(out.contains(" - "), "expected ' - ' in ASCII summary line");
        assert!(
            out.contains("|"),
            "expected '|' (mid bullet) in ASCII output"
        );
        assert!(
            out.contains("`"),
            "expected '`' (last bullet) in ASCII output"
        );
        assert!(
            out.contains(" . "),
            "expected ' . ' (middot replacement) in ASCII summary"
        );
    }

    #[test]
    fn test_render_unicode_keeps_box_chars() {
        // Unicode モードでは従来通り — / · / ├ / └ を使う (regression guard)
        let diags = vec![
            Diagnostic::new(Severity::Warn, CAT_STATE_INTEGRITY, "x", "1 found")
                .with_details(vec!["a".into(), "b".into()]),
        ];
        let icons = Icons::from_style(IconStyle::Unicode);
        let out = render(&diags, &icons);
        assert!(out.contains("\u{2014}")); // —
        assert!(out.contains("\u{00b7}")); // ·
        assert!(out.contains("\u{251c}")); // ├
        assert!(out.contains("\u{2514}")); // └
    }

    #[test]
    fn test_render_multi_detail_uses_tree_chars() {
        let diags = vec![
            Diagnostic::new(
                Severity::Warn,
                CAT_STATE_INTEGRITY,
                "unused cache dirs",
                "3 found",
            )
            .with_details(vec!["a".into(), "b".into(), "c".into()])
            .with_hint("rvpm clean"),
        ];
        let icons = Icons::from_style(IconStyle::Unicode);
        let out = render(&diags, &icons);
        // first two use ├, last uses └
        assert!(out.contains("├ a"));
        assert!(out.contains("├ b"));
        assert!(out.contains("└ c"));
        assert!(out.contains("hint: rvpm clean"));
    }

    #[test]
    fn test_render_category_ordering() {
        // Diags deliberately inserted out of order; render must emit categories in fixed order.
        let diags = vec![
            Diagnostic::new(Severity::Ok, CAT_TOOLS, "nvim", "v1"),
            Diagnostic::new(Severity::Ok, CAT_PLUGIN_CONFIG, "depends cycles", "none"),
            Diagnostic::new(Severity::Ok, CAT_NEOVIM, "init.lua loader hook", "linked"),
            Diagnostic::new(Severity::Ok, CAT_STATE_INTEGRITY, "cloned plugins", "1/1"),
        ];
        let icons = Icons::from_style(IconStyle::Unicode);
        let out = render(&diags, &icons);
        let pos_plugin = out.find("Plugin config").unwrap();
        let pos_state = out.find("State integrity").unwrap();
        let pos_nvim = out.find("Neovim integration").unwrap();
        let pos_tools = out.find("External tools").unwrap();
        assert!(pos_plugin < pos_state);
        assert!(pos_state < pos_nvim);
        assert!(pos_nvim < pos_tools);
    }

    // -------- shorten_version --------

    #[test]
    fn test_shorten_version_nvim() {
        assert_eq!(
            shorten_version("NVIM v0.13.0-dev-foo bar baz"),
            "v0.13.0-dev-foo"
        );
    }

    #[test]
    fn test_shorten_version_git() {
        assert_eq!(shorten_version("git version 2.49.1"), "v2.49.1");
    }

    #[test]
    fn test_shorten_version_chezmoi() {
        assert_eq!(
            shorten_version("chezmoi version v2.68.0, commit foo"),
            "v2.68.0"
        );
    }
}
