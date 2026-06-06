//! Path resolution and small config/log helpers.
//!
//! Centralizes how rvpm derives its `~/.config/rvpm/<appname>` /
//! `~/.cache/rvpm/<appname>` roots (and everything underneath) plus a few
//! adjacent helpers (`appname`, tilde expansion, config-flag reads, update-log
//! recording). Extracted from the former monolithic `main.rs`/`lib.rs` so the
//! `resolve_*` family lives in one navigable place (#217).

use crate::config;
use anyhow::Result;
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

// ====================================================================
// Paths: `.config` / `.cache` をクロスプラットフォームで固定する。
//
// Windows でも `dirs::config_dir()` (≒ `%APPDATA%`) ではなく明示的に
// `~/.config` / `~/.cache` を使う。理由:
//   - Neovim の config 慣習と一致 (`~/.config/nvim`)
//   - dotfiles を WSL / Linux / Windows で同じパス構造で共有できる
//   - 単一の mental model で済む
//
// ユーザー側で別のパスにしたければ TOML の options で上書きできる:
//   - options.cache_root  → 全キャッシュの root (plugins/ と browse/ が配下)
//   - options.config_root → 全コンフィグの root (config.toml / 全 global hook /
//                           plugins/ が配下)
//
// config_root と cache_root は対称構造:
//   <config_root>/config.toml
//   <config_root>/before.lua / after.lua             (global hooks)
//   <config_root>/plugins/<host>/<owner>/<repo>/     (per-plugin hooks)
//   <cache_root>/plugins/{repos,merged,loader.lua}   (plugins 本体)
//   <cache_root>/browse/                             (browse キャッシュ)
//
// $RVPM_APPNAME > $NVIM_APPNAME > "nvim" の順で appname が決まり、
// デフォルトパスの末尾に appname が入る:
//   ~/.config/rvpm/<appname>/
//   ~/.cache/rvpm/<appname>/
// ====================================================================

/// `~/.config/rvpm/config.toml` (固定)
/// $RVPM_APPNAME → $NVIM_APPNAME → "nvim" の優先順で appname を決定。
/// 無効な値 (空文字、パス区切り含む、"." / "..") は "nvim" に fallback。
pub(crate) fn appname() -> String {
    let raw = std::env::var("RVPM_APPNAME")
        .or_else(|_| std::env::var("NVIM_APPNAME"))
        .unwrap_or_default();
    if is_valid_appname(&raw) {
        raw
    } else {
        "nvim".to_string()
    }
}

/// appname が path segment として安全か検証。
pub(crate) fn is_valid_appname(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

pub(crate) fn rvpm_config_path() -> PathBuf {
    let home = dirs::home_dir().expect("Could not find home directory");
    home.join(".config")
        .join("rvpm")
        .join(appname())
        .join("config.toml")
}

/// `config.toml` から `options.chezmoi` フラグだけを軽量に読み出す。
/// `parse_config` は Tera 展開 + topological sort を行う重量級処理なので、
/// mutate 系コマンドがフラグ 1 つを見るためだけに呼ぶのは無駄。
/// toml_edit で該当キーだけ直接参照する。
/// ファイルが存在しない / パースできない / キーが無い場合は `false`。
pub(crate) fn read_chezmoi_flag(config_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(doc) = content.parse::<DocumentMut>() else {
        return false;
    };
    doc.get("options")
        .and_then(|o| o.get("chezmoi"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// config.toml が存在しなければ最小テンプレートで新規作成する。
/// 既に存在する場合は何もしない (冪等)。作成した場合は true を返す。
pub(crate) fn ensure_config_exists(config_path: &Path) -> Result<bool> {
    if config_path.exists() {
        return Ok(false);
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let template = "\
# rvpm config — https://github.com/yukimemi/rvpm#configuration
[options]
";
    std::fs::write(config_path, template)?;
    println!("Created {}", config_path.display());
    Ok(true)
}

/// doctor 用の pub 再公開 (元 `expand_tilde` を module 外から使いたいため)。
pub(crate) fn expand_tilde_public(path: &str) -> PathBuf {
    expand_tilde(path)
}

/// doctor 用の pub 再公開 (元 `init_lua_references_rvpm_loader` の同義)。
pub(crate) fn init_lua_references_rvpm_loader_public(init_lua_path: &Path) -> bool {
    crate::init_lua_references_rvpm_loader(init_lua_path)
}

/// `~` / `~/foo` / `~\foo` 形式を home dir に展開する。
/// それ以外はそのまま PathBuf に変換。
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().expect("Could not find home directory");
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return dirs::home_dir()
            .expect("Could not find home directory")
            .join(rest);
    }
    PathBuf::from(path)
}

/// rvpm のキャッシュ root を決定する。
/// `options.cache_root` が設定されていればそれを tilde 展開して返す。
/// 未設定なら `~/.cache/rvpm/<appname>` (デフォルト)。
/// この配下に `plugins/{repos,merged,loader.lua}` が配置される。
pub(crate) fn resolve_cache_root(config_cache_root: Option<&str>) -> PathBuf {
    match config_cache_root {
        Some(raw) => expand_tilde(raw),
        None => {
            let home = dirs::home_dir().expect("Could not find home directory");
            home.join(".cache").join("rvpm").join(appname())
        }
    }
}

/// config.toml / global hook / per-plugin hook の親 root を決定する。
/// `options.config_root` が設定されていればそれを tilde 展開して返す。
/// 未設定なら `~/.config/rvpm/<appname>` (デフォルト)。
pub(crate) fn resolve_config_root(config_root: Option<&str>) -> PathBuf {
    match config_root {
        Some(raw) => expand_tilde(raw),
        None => {
            let home = dirs::home_dir().expect("Could not find home directory");
            home.join(".config").join("rvpm").join(appname())
        }
    }
}

/// 指定プラグインの per-plugin hook ディレクトリ (`<config_root>/plugins/<host>/<owner>/<repo>`)
/// を返す。
pub(crate) fn resolve_plugin_config_dir(config_root: &Path, plugin: &config::Plugin) -> PathBuf {
    config_root.join("plugins").join(plugin.canonical_path())
}

/// loader.lua のパス。常に `<cache_root>/plugins/loader.lua`。
pub(crate) fn resolve_loader_path(cache_root: &Path) -> PathBuf {
    cache_root.join("plugins").join("loader.lua")
}

/// lockfile (`rvpm.lock`) のパス。dotfiles にコミットされる想定のため、
/// `cache_root` ではなく `config_root` (= `~/.config/rvpm/<appname>/`) 直下。
/// `options.config_root` が上書きされていればそれに追従する。
pub(crate) fn resolve_lockfile_path(config_root: &Path) -> PathBuf {
    config_root.join("rvpm.lock")
}

/// `rvpm log` の永続化先。常に `<cache_root>/update_log.json` (loader.lua の親
/// ディレクトリと同階層 = cache_root 直下、`plugins/` 配下ではない)。
pub(crate) fn resolve_update_log_path(cache_root: &Path) -> PathBuf {
    cache_root.join("update_log.json")
}

/// `rvpm doctor` が読む最新 sync の merge 衝突スナップショット。
/// `<cache_root>/merge_conflicts.json` 固定 (`update_log.json` と同じ場所)。
pub(crate) fn resolve_merge_conflicts_path(cache_root: &Path) -> PathBuf {
    cache_root.join("merge_conflicts.json")
}

/// プラグイン単位の最終 fetch 時刻キャッシュ。
/// `<cache_root>/fetch_state.json` 固定 (update_log / merge_conflicts と同じ場所)。
pub(crate) fn resolve_fetch_state_path(cache_root: &Path) -> PathBuf {
    cache_root.join("fetch_state.json")
}

/// `Plugin` + `GitChange` から永続化向けの `ChangeRecord` を組み立てる小ヘルパー。
/// run_sync / run_update / run_add で共通利用。
pub(crate) fn change_record_from(
    plugin: &crate::config::Plugin,
    change: crate::git::GitChange,
) -> crate::update_log::ChangeRecord {
    crate::update_log::ChangeRecord {
        name: plugin.display_name(),
        url: plugin.url.clone(),
        from: change.from,
        to: change.to,
        subjects: change.subjects,
        breaking_subjects: change.breaking_subjects,
        doc_files_changed: change.doc_files_changed,
    }
}

/// `record_run` を呼び、書き込み失敗時は警告を出すだけで panic / 操作中断はしない。
/// 1 件の git 操作も発生しなかった (= changes が空) 場合でも run の事実は残す。
pub(crate) fn record_changes_or_warn(
    cache_root: &Path,
    command: &str,
    changes: Vec<crate::update_log::ChangeRecord>,
) {
    let path = resolve_update_log_path(cache_root);
    if let Err(e) = crate::update_log::record_run(&path, command, changes) {
        eprintln!(
            "\u{26a0} update_log: failed to record {} run at {}: {}",
            command,
            path.display(),
            e
        );
    }
}

/// repos の親ディレクトリ。`<cache_root>/plugins/repos`。
pub(crate) fn resolve_repos_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("plugins").join("repos")
}

/// merged ディレクトリ。`<cache_root>/plugins/merged`。
pub(crate) fn resolve_merged_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("plugins").join("merged")
}

/// per-plugin view ディレクトリのルート。`<cache_root>/plugins/views`。
/// `merge=true && eager` 以外の全プラグインがここに自身の rtp tree を持つ (#119)。
pub(crate) fn resolve_views_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("plugins").join("views")
}

/// 個別プラグインの view ディレクトリ。
/// `<cache_root>/plugins/views/<host>/<owner>/<repo>/`。
pub(crate) fn resolve_plugin_view_dir(views_dir: &Path, plugin: &crate::config::Plugin) -> PathBuf {
    views_dir.join(plugin.canonical_path())
}
