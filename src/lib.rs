mod ai;
mod browse;
mod browse_tui;
mod chezmoi;
mod cli;
mod commands;
mod config;
mod doctor;
mod external_render;
mod fetch_state;
mod git;
mod helptags;
mod link;
mod loader;
mod lockfile;
mod merge;
mod merge_conflicts;
mod paths;
mod plugin_build;
mod plugin_scan;
mod profile;
mod profile_tui;
mod self_update;
mod tui;
mod update_log;
mod url;
mod view_stamp;

use crate::git::Repo;
use crate::loader::generate_loader;
// Path resolution + small config/log helpers now live in `paths` (#217).
// Glob-import them so the many bare call sites in this file are unchanged,
// and re-export the two doctor-facing wrappers so `crate::*_public` keeps
// resolving for `src/doctor.rs`.
use crate::paths::*;
pub(crate) use crate::paths::{expand_tilde_public, init_lua_references_rvpm_loader_public};
// Merge mode lives in `merge` (#217); re-export the public `PluginMergeMode`
// enum so `rvpm::PluginMergeMode` stays a valid path. The internal call sites
// that used the merge/url/plugin_build globs all moved to `commands`.
pub use crate::merge::PluginMergeMode;
// `run()` plus the clap `Cli` / `Commands` definitions and the `run_cli`
// dispatch now live in `cli` (#233); re-export `run` so `rvpm::run()` and
// `src/main.rs` (`fn main() { rvpm::run() }`) stay unchanged. `Cli` is
// re-exported at crate visibility so `commands::run_completion` and the
// completion-generation test keep resolving it via `use crate::*` / `super::*`.
pub(crate) use crate::cli::Cli;
pub use crate::cli::run;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::tui::PluginStatus;
use tokio::sync::mpsc;

/// `cond` 指定時の整合性を取るプリパス (#119)。
/// - `merge=true` を `false` に強制 (cond=false のとき merged rtp に中身が残る矛盾を防ぐ)
/// - `merge_doc` は per-plugin が **明示指定なし (None)** のときだけ `Some(false)` 化する。
///   ユーザーが per-plugin で `merge_doc = true` を明示した場合は cond でも尊重し、
///   "Windows 限定 plugin だけど help は引きたい" ようなケースを許す。
///   global `options.merge_doc = true` の sweep が cond plugin を巻き込むのを止めたい場合
///   は per-plugin で `merge_doc = false` を明示 (こちらも `None` でない値なので尊重される)。
fn disable_merge_if_cond(plugin: &mut crate::config::Plugin) {
    if plugin.cond.is_some() {
        if plugin.merge {
            plugin.merge = false;
        }
        if plugin.merge_doc.is_none() {
            plugin.merge_doc = Some(false);
        }
    }
}

/// プラグインの clone 先パスを解決する。
/// `plugin.dst` が `~/...` 形式の場合は home dir に展開する (dev プラグインで頻出)。
pub(crate) fn resolve_plugin_dst(plugin: &crate::config::Plugin, cache_root: &Path) -> PathBuf {
    if let Some(d) = &plugin.dst {
        expand_tilde(d)
    } else {
        resolve_repos_dir(cache_root).join(plugin.canonical_path())
    }
}

/// `query` で url を fuzzy / 部分一致で 1 件選ぶ、または非対話なら FuzzySelect TUI を出す。
///
/// 戻り値:
///   - `Ok(Some(url))` — 選択成功
///   - `Ok(None)`     — interactive 選択で user が ESC キャンセルした
///   - `Err(_)`       — `query` 指定時に該当 plugin が無かった (Plugin not found)
///
/// `run_remove` / `run_set` / `run_tune` に同じ if/else ブロックが散らばっていたのを
/// 共通化したもの (Gemini PR #100 review 指摘)。caller 側でキャンセル時の戻り値が
/// 違うので (`Ok(())` / `Ok(false)` / etc) `Option` で受けて呼び元が分岐する。
fn select_plugin_url(
    plugins: &[crate::config::Plugin],
    query: Option<&str>,
    prompt: &str,
) -> Result<Option<String>> {
    if let Some(q) = query {
        // 曖昧 partial match を防ぐ (CodeRabbit PR #100 指摘):
        // 複数の plugin を含む query を黙って先頭採用すると、`rvpm tune cmp`
        // で `cmp-buffer` / `cmp-cmdline` ... のうちどれが書き換えられるか
        // 予測不能になり、mutating コマンドで重大事故が起きる。
        //
        // 解決順序:
        //   1. 完全一致 (`p.url == q`) があれば即採用 (1 個だけ通過)。
        //   2. 部分一致が 1 件 → 採用。0 件 → "Plugin not found"。
        //   3. 部分一致が複数 → match 一覧を見せて refine を促す error。
        if let Some(p) = plugins.iter().find(|p| p.url == q) {
            return Ok(Some(p.url.clone()));
        }
        let partial: Vec<&str> = plugins
            .iter()
            .filter(|p| p.url.contains(q))
            .map(|p| p.url.as_str())
            .collect();
        match partial.len() {
            0 => Err(anyhow::anyhow!("Plugin not found")),
            1 => Ok(Some(partial[0].to_string())),
            _ => {
                let listing = partial
                    .iter()
                    .map(|u| format!("  - {u}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Err(anyhow::anyhow!(
                    "Query '{q}' matches multiple plugins; refine your query or omit it to pick interactively:\n{listing}"
                ))
            }
        }
    } else {
        let urls: Vec<String> = plugins.iter().map(|p| p.url.clone()).collect();
        let selection = FuzzySelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt(prompt)
            .default(0)
            .items(&urls)
            .interact_opt()?;
        Ok(selection.map(|idx| urls[idx].clone()))
    }
}

/// A plugin that sync left at a lockfile-pinned commit while its remote
/// has advanced. Reported in the sync summary so users know `rvpm update`
/// would actually change something.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HeldBackPin {
    name: String,
    pinned: String,
    remote: String,
}

/// Decide whether a just-synced plugin is being "held back" by a lockfile
/// pin. Returns `Some((pinned_commit, remote_tip))` when:
///
/// - the plugin has **no explicit `rev`** (not the user's choice),
/// - the **lockfile contributed** an effective rev (pin came from the lockfile),
/// - and the pinned HEAD is **behind the remote tracking tip**.
///
/// Returns `None` otherwise — explicit `rev`, no lockfile contribution,
/// already at remote tip, or either commit unknown (treat unknowns as
/// "not held back" to avoid false positives).
///
/// This is the pure classification step; the caller is responsible for
/// composing the plugin display name and emitting the summary line.
fn classify_held_back(
    plugin_rev: Option<&str>,
    effective_rev: Option<&str>,
    head_commit: Option<&str>,
    remote_head: Option<&str>,
) -> Option<(String, String)> {
    if plugin_rev.is_some() {
        return None;
    }
    effective_rev?;
    let head = head_commit?;
    let remote = remote_head?;
    if head != remote {
        Some((head.to_string(), remote.to_string()))
    } else {
        None
    }
}

/// 現在の plugin が `--rebuild [QUERY]` のスコープに入るか判定する。
///
/// `rebuild_filter_lc` は **呼び出し側で小文字化済み** の前提。`run_sync` は N
/// プラグインを回すので、N 回同じ query を lowercase するコストを避けるため
/// 事前正規化を外側に持たせている。
///
/// - `None` (フラグ未指定) → 常に false (build 強制しない)
/// - `Some("")` (フラグのみ、値無し) → 常に true (全 plugin)
/// - `Some(q)` (フラグ + query) → plugin の url / name の
///   いずれかに q が含まれれば true。url 側で決着すれば name の lowercase
///   allocation は走らない (短絡評価)。
fn matches_rebuild_filter(plugin: &crate::config::Plugin, rebuild_filter_lc: Option<&str>) -> bool {
    match rebuild_filter_lc {
        None => false,
        Some("") => true,
        Some(qlc) => {
            plugin.url.to_ascii_lowercase().contains(qlc)
                || plugin
                    .name
                    .as_deref()
                    .is_some_and(|n| n.to_ascii_lowercase().contains(qlc))
        }
    }
}

pub(crate) async fn update_single_plugin(
    plugin: &crate::config::Plugin,
    cache_root: &Path,
    tx: mpsc::Sender<(String, PluginStatus)>,
) -> Result<(
    crate::config::Plugin,
    Option<crate::git::GitChange>,
    Option<String>,
)> {
    let dst_path = resolve_plugin_dst(plugin, cache_root);
    let is_missing = !dst_path.exists();
    let _ = tx
        .send((
            plugin.url.clone(),
            PluginStatus::Syncing(if is_missing {
                "Syncing...".to_string()
            } else {
                "Updating...".to_string()
            }),
        ))
        .await;
    let repo = Repo::new(&plugin.url, &dst_path, plugin.rev.as_deref());
    let res = if is_missing {
        repo.sync().await
    } else {
        repo.update().await
    };
    match res {
        Ok(change) => {
            let head_commit = repo.head_commit().await.ok();
            let _ = tx.send((plugin.url.clone(), PluginStatus::Finished)).await;
            Ok((plugin.clone(), change, head_commit))
        }
        Err(e) => {
            let _ = tx
                .send((plugin.url.clone(), PluginStatus::Failed(e.to_string())))
                .await;
            Err(e)
        }
    }
}

use toml_edit::{DocumentMut, value};

/// `rvpm add` の scan 後に決まる、config.toml に書き込むべき trigger 候補。
struct AddTriggerSuggestion {
    /// on_cmd に入れる文字列 (exact 名 + `/regex/` の mixed list、ソート済)。
    on_cmd: Vec<String>,
    /// on_map に入れる候補 (lhs の enumerate のみ、regex 提案なし: 記号混じりで LCP 無意味)。
    on_map: Vec<crate::config::MapSpec>,
}

impl AddTriggerSuggestion {
    fn is_empty(&self) -> bool {
        self.on_cmd.is_empty() && self.on_map.is_empty()
    }
}

/// 非 TTY script でも `rvpm add --auto-lazy` が安定動作するよう、TTY 判定は
/// `stdin` / `stdout` の両方を見る。どちらかでも非 TTY なら prompt を出さない。
fn is_interactive_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// scan 結果から suggestion を組み立てる。0 件なら `None` — 提案する中身が無い。
fn build_add_suggestion(scan: &crate::plugin_scan::ScanResult) -> Option<AddTriggerSuggestion> {
    let on_cmd = crate::plugin_scan::suggest_cmd_triggers_smart(&scan.commands, 3);
    // on_map は regex 化せず enumerate のみ (lhs に記号混じりで LCP 無意味)。
    let on_map: Vec<crate::config::MapSpec> = scan
        .user_maps
        .iter()
        .map(|m| crate::config::MapSpec {
            lhs: m.lhs.clone(),
            mode: m.modes.clone(),
            desc: None,
        })
        .collect();
    let s = AddTriggerSuggestion { on_cmd, on_map };
    if s.is_empty() { None } else { Some(s) }
}

/// 対話プロンプトで user に選ばせて、適用する trigger を返す。
///   - `Some(suggestion)` → 適用
///   - `None`             → eager のまま
fn prompt_lazy_decision(
    display_name: &str,
    suggestion: &AddTriggerSuggestion,
) -> Option<AddTriggerSuggestion> {
    use dialoguer::{Select, theme::ColorfulTheme};

    println!();
    println!("[{}] detected lazy triggers:", display_name);
    if !suggestion.on_cmd.is_empty() {
        println!("  on_cmd = {}", toml_array_preview(&suggestion.on_cmd));
    }
    if !suggestion.on_map.is_empty() {
        let lhs: Vec<String> = suggestion.on_map.iter().map(|m| m.lhs.clone()).collect();
        println!("  on_map = {}", toml_array_preview(&lhs));
    }

    let choices = ["accept (lazy-load)", "skip (eager install)"];
    let sel = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("How should rvpm install this plugin?")
        .items(choices.as_slice())
        .default(0)
        .interact()
        .ok()?;

    match sel {
        0 => Some(AddTriggerSuggestion {
            on_cmd: suggestion.on_cmd.clone(),
            on_map: suggestion.on_map.clone(),
        }),
        _ => None,
    }
}

fn toml_array_preview(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("\"{}\"", s)).collect();
    format!("[{}]", quoted.join(", "))
}

/// `rvpm add` 時の effective policy を決定:
///   - CLI `--lazy` / `--no-lazy` (policy_override) が最優先
///   - なければ `config.options.auto_lazy`
fn resolve_add_lazy_policy(
    policy_override: Option<crate::config::AutoLazyPolicy>,
    config: &crate::config::Config,
) -> crate::config::AutoLazyPolicy {
    policy_override.unwrap_or(config.options.auto_lazy)
}

/// scan 結果 + policy に基づいて suggestion を適用するか決める。
/// Never → None、Always → そのまま採用、Ask → TTY なら prompt / 非 TTY なら skip。
fn decide_add_lazy_apply(
    suggestion: AddTriggerSuggestion,
    policy: crate::config::AutoLazyPolicy,
    display_name: &str,
) -> Option<AddTriggerSuggestion> {
    use crate::config::AutoLazyPolicy;
    match policy {
        AutoLazyPolicy::Never => None,
        AutoLazyPolicy::Always => Some(suggestion),
        AutoLazyPolicy::Ask => {
            if is_interactive_tty() {
                prompt_lazy_decision(display_name, &suggestion)
            } else {
                None
            }
        }
    }
}

/// `replace_plugin_entry_with_ai_toml` の挙動切替。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeMode {
    /// **Merge** (additive). 既存 entry に AI 提案 key を上書き / 追加するが、
    /// 提案に無い既存 key は保持。`rvpm add --ai` 用 — stub entry には CLI flag で
    /// 入れた値しか無いので merge も replace も結果は同じだが、
    /// 安全側に倒している。
    Merge,
    /// **Replace** (destructive). AI 提案に無い既存 key も `preserved_keys` で
    /// 明示保護されていない限り削除。`rvpm tune` 用 — user は「AI に全部書き直して」
    /// と頼んでいるので、AI が omit した stale field (`on_cmd`, `rev` 等) も
    /// 落とすのが期待値 (CodeRabbit PR #100 指摘)。`url` は `preserved_keys` 扱いで
    /// 常に `stored_url` に強制リセット。
    Replace,
}

/// AI mode 用 (#93): AI が返した `[[plugins]]` ブロック (1 entry のみ) を、既存
/// entry に書き込む。`mode` で merge / destructive replace を切り替える。
///
/// 設計ポイント:
///   - **stub entry の文書中の位置 / 装飾 (改行・空白) を保つ** — `remove + insert`
///     方式は toml_edit が新 entry を `[vars]` の直後など想定外の位置にレンダリング
///     してしまう (user 報告: 既存 [[plugins]] の末尾ではなく `[vars]` 直後に配置される)。
///   - **user の明示 CLI flag を尊重** — `preserved_keys` に挙げたキーは AI 提案で
///     上書きしない (`rvpm add owner/repo --rev v1.0 --ai claude` で `--rev` を残す)。
///     `url` は `stored_url` に強制リセット (canonical 化)。
///   - **`Replace` mode** では更に、`preserved_keys` にも proposal にも無い既存 key を
///     明示削除する。これが無いと `tune` で AI が "この field はもう不要だから消して"
///     と返したつもりが反映されない (CodeRabbit PR #100 指摘)。
fn replace_plugin_entry_with_ai_toml(
    doc: &mut DocumentMut,
    stored_url: &str,
    proposal_toml: &str,
    preserved_keys: &[&str],
    mode: MergeMode,
) -> anyhow::Result<()> {
    use anyhow::{Context, anyhow};

    let proposal_doc: DocumentMut = proposal_toml
        .parse::<DocumentMut>()
        .context("AI proposal TOML failed to parse")?;
    let proposal_plugins = proposal_doc
        .get("plugins")
        .and_then(|p| p.as_array_of_tables())
        .ok_or_else(|| anyhow!("AI proposal missing [[plugins]] array"))?;
    if proposal_plugins.len() != 1 {
        return Err(anyhow!(
            "AI proposal must contain exactly 1 plugin entry; got {}",
            proposal_plugins.len()
        ));
    }
    let new_entry = proposal_plugins.get(0).unwrap();

    // 既存 doc の plugins 配列から url 一致を探して in-place マージ。
    let plugins = doc
        .get_mut("plugins")
        .and_then(|p| p.as_array_of_tables_mut())
        .ok_or_else(|| anyhow!("config.toml missing [[plugins]] array"))?;
    for i in 0..plugins.len() {
        let existing_url = plugins
            .get(i)
            .and_then(|t| t.get("url"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if existing_url != stored_url {
            continue;
        }
        let existing = plugins.get_mut(i).unwrap();

        // Replace mode: AI 提案にも preserved_keys にも無い既存 key を消す。
        // url は preserved 扱いで常に保持される (下で強制リセットされるが、ここでは消さない)。
        if mode == MergeMode::Replace {
            let proposal_keys: std::collections::HashSet<&str> =
                new_entry.iter().map(|(k, _)| k).collect();
            let to_remove: Vec<String> = existing
                .iter()
                .map(|(k, _)| k.to_string())
                .filter(|k| {
                    k != "url"
                        && !preserved_keys.contains(&k.as_str())
                        && !proposal_keys.contains(k.as_str())
                })
                .collect();
            for k in to_remove {
                existing.remove(&k);
            }
        }

        // AI 提案の各キーを既存 entry に書き込む。
        // ただし `preserved_keys` (user が CLI flag で明示したもの) は skip。
        // url は最後に強制セット。
        for (key, value_item) in new_entry.iter() {
            if key == "url" {
                continue;
            }
            if preserved_keys.contains(&key) {
                continue;
            }
            existing[key] = value_item.clone();
        }
        existing["url"] = value(stored_url);
        return Ok(());
    }
    Err(anyhow!(
        "could not find stub [[plugins]] entry with url=`{stored_url}` in config.toml"
    ))
}

/// 既存 config.toml 内の `[[plugins]]` のうち url が一致するエントリに
/// `on_cmd` / `on_map` を書き込む。toml_edit で in-place patch。
fn patch_plugin_entry_triggers(
    doc: &mut DocumentMut,
    stored_url: &str,
    applied: &AddTriggerSuggestion,
) {
    let Some(plugins) = doc
        .get_mut("plugins")
        .and_then(|p| p.as_array_of_tables_mut())
    else {
        return;
    };
    for t in plugins.iter_mut() {
        let url = t.get("url").and_then(|v| v.as_str()).unwrap_or_default();
        if url != stored_url {
            continue;
        }
        // 既存 on_cmd / on_map がある場合は user が CLI フラグ (`--on-cmd …`) で
        // 明示指定した or 過去の add / 手編集で既に書いていたもの。scan 結果で
        // 上書きすると user 設定を破壊するので skip (PR #91 review 指摘)。
        if !applied.on_cmd.is_empty() && t.get("on_cmd").is_none() {
            let mut arr = toml_edit::Array::new();
            for s in &applied.on_cmd {
                arr.push(s.as_str());
            }
            t["on_cmd"] = value(arr);
        }
        if !applied.on_map.is_empty() && t.get("on_map").is_none() {
            // 全 entry が default mode (n) なら string 配列、それ以外なら table 配列。
            // MapSpec::mode の default は ["n"]。modes が `["n"]` ジャストなら string で十分。
            let all_default = applied
                .on_map
                .iter()
                .all(|m| m.mode == vec!["n".to_string()]);
            if all_default {
                let mut arr = toml_edit::Array::new();
                for m in &applied.on_map {
                    arr.push(m.lhs.as_str());
                }
                t["on_map"] = value(arr);
            } else {
                let mut arr = toml_edit::Array::new();
                for m in &applied.on_map {
                    let mut tb = toml_edit::InlineTable::new();
                    tb.insert("lhs", m.lhs.as_str().into());
                    let mut modes_arr = toml_edit::Array::new();
                    for md in &m.mode {
                        modes_arr.push(md.as_str());
                    }
                    tb.insert("mode", toml_edit::Value::from(modes_arr));
                    arr.push(toml_edit::Value::InlineTable(tb));
                }
                t["on_map"] = value(arr);
            }
        }
        break;
    }
}

use dialoguer::FuzzySelect;

/// 英語の単数/複数形切替。表示メッセージで使う小さなヘルパー。
///
/// `n == 1` のときだけ単数形を返し、それ以外（0 を含む）は複数形を返す。
///
/// ```
/// assert_eq!(rvpm::plural("plugin", "plugins", 1), "plugin");
/// assert_eq!(rvpm::plural("plugin", "plugins", 0), "plugins");
/// assert_eq!(rvpm::plural("plugin", "plugins", 3), "plugins");
/// ```
pub fn plural<'a>(singular: &'a str, plural: &'a str, n: usize) -> &'a str {
    if n == 1 { singular } else { plural }
}

/// `sync --prune` / `generate` (auto_clean) / 両方の末尾で使う共通の「後片付け」。
/// 未使用 repo を検出し、`force` が true なら `prune_unused_repos` で削除する。
/// 戻り値は検出された未使用の件数。0 以外なら呼び出し側で警告メッセージを
/// 出せるよう、発見はしたが削除していないケースを区別できるようにする。
fn maybe_prune_unused_repos(
    config: &config::Config,
    cache_root: &Path,
    force: bool,
) -> (usize, Vec<PathBuf>) {
    let repos_dir = resolve_repos_dir(cache_root);
    if !repos_dir.exists() {
        return (0, Vec::new());
    }
    let unused = find_unused_repos(config, cache_root, &repos_dir).unwrap_or_default();
    if unused.is_empty() {
        return (0, Vec::new());
    }
    let count = unused.len();
    if force {
        prune_unused_repos(&unused);
        (count, Vec::new()) // 削除済みなのでパスは返さない
    } else {
        (count, unused)
    }
}

/// `views/<host>/<owner>/<repo>/` 配下で、 期待されない (= 現 sync で
/// `views/` 経由になっていない / config から消えた) plugin の view を削除する (#119)。
///
/// `expected_views`: 今回 sync / generate で実際に build_view した path の集合 (絶対 path)。
///
/// 集合に含まれない view subdirectory を削除する。 削除失敗は warn 出すだけで続行
/// (resilience)。 promote_lazy_to_eager で View → Full に切り替わった plugin の
/// stale view も自動的に消える。
fn prune_stale_views(views_dir: &Path, expected_views: &std::collections::HashSet<PathBuf>) {
    if !views_dir.exists() {
        return;
    }
    // views/<host>/<owner>/<repo>/ の 3 階層 fixed depth を walk
    // (canonical_path 形式に従う)。
    let host_iter = match std::fs::read_dir(views_dir) {
        Ok(it) => it,
        Err(_) => return,
    };
    for host_entry in host_iter.flatten() {
        let host_path = host_entry.path();
        if !host_path.is_dir() {
            continue;
        }
        let owner_iter = match std::fs::read_dir(&host_path) {
            Ok(it) => it,
            Err(_) => continue,
        };
        let mut host_empty = true;
        for owner_entry in owner_iter.flatten() {
            let owner_path = owner_entry.path();
            if !owner_path.is_dir() {
                continue;
            }
            let repo_iter = match std::fs::read_dir(&owner_path) {
                Ok(it) => it,
                Err(_) => continue,
            };
            let mut owner_empty = true;
            for repo_entry in repo_iter.flatten() {
                let repo_path = repo_entry.path();
                if !repo_path.is_dir() {
                    continue;
                }
                if expected_views.contains(&repo_path) {
                    owner_empty = false;
                    host_empty = false;
                    continue;
                }
                if let Err(e) = std::fs::remove_dir_all(&repo_path) {
                    eprintln!(
                        "\u{26a0} failed to prune stale view {}: {}",
                        repo_path.display(),
                        e
                    );
                    owner_empty = false;
                    host_empty = false;
                }
            }
            // 空になった owner ディレクトリも削除 (best-effort)
            if owner_empty {
                let _ = std::fs::remove_dir(&owner_path);
            } else {
                host_empty = false;
            }
        }
        if host_empty {
            let _ = std::fs::remove_dir(&host_path);
        }
    }
}

/// 未使用 repo ディレクトリを削除する共通処理。`sync --prune` と `clean` 両方から呼ばれる。
/// 削除失敗は eprintln で警告のみ出し、処理を続ける (resilience 原則)。
fn prune_unused_repos(unused: &[PathBuf]) {
    println!();
    println!(
        "Pruning {} unused plugin {}:",
        unused.len(),
        plural("directory", "directories", unused.len()),
    );
    for path in unused {
        println!("  - {}", path.display());
        if let Err(e) = std::fs::remove_dir_all(path) {
            eprintln!("    \u{26a0} failed: {}", e);
        }
    }
}

/// `{cache_root}/plugins/repos/` 配下で、config.toml に載っていないプラグイン
/// ディレクトリを列挙する (削除候補)。
///
/// 判定ルール:
/// - 使用中セットには `resolve_plugin_dst()` の結果 (= `plugin.dst` があれば
///   それ、無ければ canonical_path ベースの既定) のうち `repos_dir` 配下の
///   ものだけを入れる。`dst` を別ツリーに逃がしてる plugin は対象外になる。
/// - `.git` を持つ候補ディレクトリを検出し、**使用中パス自身またはその
///   子孫であれば保護する** (= プラグイン本体の clone の中に置かれた
///   submodule の `.git` を誤削除しない)。
fn find_unused_repos(
    config: &config::Config,
    cache_root: &Path,
    repos_dir: &Path,
) -> Result<Vec<PathBuf>> {
    let mut unused = Vec::new();
    let mut used_paths: Vec<PathBuf> = Vec::new();
    for plugin in &config.plugins {
        let dst = resolve_plugin_dst(plugin, cache_root);
        // custom dst がツリー外 (`~/dev/...` 等) の場合は repos_dir のスキャン
        // 対象外なので used 判定にも不要。
        if dst.starts_with(repos_dir) {
            used_paths.push(dst);
        }
    }
    for entry in walkdir::WalkDir::new(repos_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() == ".git")
    {
        let git_dir = entry.path();
        if let Some(repo_root) = git_dir.parent()
            && !used_paths
                .iter()
                .any(|used| repo_root == used || repo_root.starts_with(used))
        {
            unused.push(repo_root.to_path_buf());
        }
    }
    Ok(unused)
}

fn remove_plugin_from_toml(doc: &mut DocumentMut, url: &str) -> Result<()> {
    let plugins = doc["plugins"]
        .as_array_of_tables_mut()
        .context("plugins is not an array of tables")?;
    let idx = plugins
        .iter()
        .position(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
        .context("Plugin not found in config")?;
    plugins.remove(idx);
    Ok(())
}

/// 既存 config.toml の DocumentMut から、指定 url の `[[plugins]]` entry を
/// **TOML テキストとして** 抜き出す (AI tune の prompt に貼るため)。
///
/// 単純に `existing.to_string()` だと `[[plugins]]` ヘッダが付かないので、
/// `[[plugins]]\n` を頭に明示的に貼って、その後に table の各 key/value を
/// 通常レンダリングで連結する。toml_edit が table 内の元 formatting (空白 /
/// 改行 / コメント) をできる限り保つので、user が手で書いた config の見た目を
/// AI に正しく見せられる。
fn extract_plugin_entry_toml(doc: &DocumentMut, url: &str) -> Option<String> {
    let plugins = doc.get("plugins").and_then(|p| p.as_array_of_tables())?;
    let entry = plugins
        .iter()
        .find(|t| t.get("url").and_then(|v| v.as_str()) == Some(url))?;
    // `[[plugins]]` header を含めて build。
    let mut out = String::from("[[plugins]]\n");
    out.push_str(&entry.to_string());
    Some(out)
}

/// 指定プラグインの任意のリスト型フィールド (on_cmd / on_ft / on_map / on_event / on_path / on_source 等) を設定する。
/// 要素が1つの場合は文字列として、2つ以上の場合は配列として書き込む (TOML の string | string[] を活用)。
fn set_plugin_list_field(
    doc: &mut DocumentMut,
    url: &str,
    field: &str,
    values: Vec<String>,
) -> Result<()> {
    let plugins = doc["plugins"]
        .as_array_of_tables_mut()
        .context("plugins is not an array of tables")?;
    let plugin_table = plugins
        .iter_mut()
        .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
        .context("Could not find plugin in toml_edit document")?;
    if values.len() == 1 {
        plugin_table[field] = value(values.into_iter().next().unwrap());
    } else {
        let mut array = toml_edit::Array::new();
        for v in values {
            array.push(v);
        }
        plugin_table[field] = value(array);
    }
    Ok(())
}

/// `--on-cmd` / `--on-ft` / `--on-event` / `--on-path` / `--on-source` の
/// 入力文字列を `Vec<String>` に正規化する。
///
/// 受け付ける形式:
/// - `"Foo"`                 → `["Foo"]`
/// - `"Foo,Bar,Baz"`         → `["Foo", "Bar", "Baz"]` (空要素は無視)
/// - `'["Foo", "Bar"]'`      → `["Foo", "Bar"]` (JSON 配列)
///
/// JSON っぽく `[` で始まっていて parse に失敗すると明示エラー。
fn parse_cli_string_list(input: &str) -> Result<Vec<String>> {
    let trimmed = input.trim();
    if trimmed.starts_with('[') {
        return serde_json::from_str::<Vec<String>>(trimmed)
            .with_context(|| format!("invalid JSON string array: {}", trimmed));
    }
    Ok(trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// `--on-map` CLI flag の入力を `Vec<MapSpec>` に変換する。
///
/// 受け付ける形式 (すべて同じ flag で混在可能):
/// - `"<leader>f"`                       (単純な文字列)
/// - `"<leader>f, <leader>g"`            (カンマ区切り)
/// - `'["<leader>f", "<leader>g"]'`      (JSON 文字列配列)
/// - `'{ "lhs": "<space>d", "mode": ["n", "x"], "desc": "..." }'`  (JSON object 単体)
/// - `'[{ ... }, "<leader>f", { ... }]'`  (JSON mixed array)
fn parse_on_map_cli(input: &str) -> Result<Vec<crate::config::MapSpec>> {
    let trimmed = input.trim();
    let first = trimmed.chars().next().unwrap_or(' ');

    // JSON 解析を試みる (配列 or オブジェクト先頭)
    if first == '[' || first == '{' {
        let value: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("invalid JSON for --on-map: {}", trimmed))?;
        return match value {
            serde_json::Value::Array(items) => items
                .into_iter()
                .map(map_spec_from_json_value)
                .collect::<Result<Vec<_>>>(),
            serde_json::Value::Object(_) => Ok(vec![map_spec_from_json_value(value)?]),
            _ => anyhow::bail!("--on-map JSON must be an object or array"),
        };
    }

    // 単純: カンマ区切り (空要素は無視) → 全部 lhs のみの MapSpec
    Ok(trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|lhs| crate::config::MapSpec {
            lhs,
            mode: Vec::new(),
            desc: None,
        })
        .collect())
}

fn map_spec_from_json_value(value: serde_json::Value) -> Result<crate::config::MapSpec> {
    use crate::config::MapSpec;
    match value {
        serde_json::Value::String(lhs) => Ok(MapSpec {
            lhs,
            mode: Vec::new(),
            desc: None,
        }),
        serde_json::Value::Object(map) => {
            let lhs = map
                .get("lhs")
                .and_then(|v| v.as_str())
                .map(String::from)
                .context("map spec missing required `lhs` field")?;
            let mode = match map.get("mode") {
                Some(serde_json::Value::String(s)) => vec![s.clone()],
                Some(serde_json::Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
                Some(_) => anyhow::bail!("`mode` must be a string or array of strings"),
                None => Vec::new(),
            };
            let desc = map.get("desc").and_then(|v| v.as_str()).map(String::from);
            Ok(MapSpec { lhs, mode, desc })
        }
        _ => anyhow::bail!("map spec must be a string or object"),
    }
}

/// `Vec<MapSpec>` を TOML の `on_map` フィールドに書き込む。
/// - 1 要素かつ simple (mode/desc なし) → plain string
/// - それ以外 → 配列 (要素ごとに simple なら string、詳細なら inline table)
fn set_plugin_map_field(
    doc: &mut DocumentMut,
    url: &str,
    specs: Vec<crate::config::MapSpec>,
) -> Result<()> {
    let plugins = doc["plugins"]
        .as_array_of_tables_mut()
        .context("plugins is not an array of tables")?;
    let plugin_table = plugins
        .iter_mut()
        .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
        .context("Could not find plugin in toml_edit document")?;

    let is_simple = |s: &crate::config::MapSpec| s.mode.is_empty() && s.desc.is_none();

    if specs.len() == 1 && is_simple(&specs[0]) {
        plugin_table["on_map"] = value(specs.into_iter().next().unwrap().lhs);
        return Ok(());
    }

    let mut array = toml_edit::Array::new();
    for spec in specs {
        if is_simple(&spec) {
            array.push(spec.lhs);
        } else {
            let mut inline = toml_edit::InlineTable::new();
            inline.insert("lhs", spec.lhs.into());
            if !spec.mode.is_empty() {
                let mut mode_arr = toml_edit::Array::new();
                for m in spec.mode {
                    mode_arr.push(m);
                }
                inline.insert("mode", toml_edit::Value::Array(mode_arr));
            }
            if let Some(desc) = spec.desc {
                inline.insert("desc", desc.into());
            }
            array.push(toml_edit::Value::InlineTable(inline));
        }
    }
    plugin_table["on_map"] = value(array);
    Ok(())
}

fn update_plugin_config(
    doc: &mut DocumentMut,
    url: &str,
    lazy: Option<bool>,
    merge: Option<bool>,
    on_cmd: Option<Vec<String>>,
    on_ft: Option<Vec<String>>,
    rev: Option<String>,
) -> Result<()> {
    if let Some(l) = lazy {
        let plugins = doc["plugins"]
            .as_array_of_tables_mut()
            .context("plugins is not an array of tables")?;
        let plugin_table = plugins
            .iter_mut()
            .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
            .context("Could not find plugin in toml_edit document")?;
        plugin_table["lazy"] = value(l);
    }
    if let Some(m) = merge {
        let plugins = doc["plugins"]
            .as_array_of_tables_mut()
            .context("plugins is not an array of tables")?;
        let plugin_table = plugins
            .iter_mut()
            .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
            .context("Could not find plugin in toml_edit document")?;
        plugin_table["merge"] = value(m);
    }
    if let Some(cmds) = on_cmd {
        set_plugin_list_field(doc, url, "on_cmd", cmds)?;
    }
    if let Some(fts) = on_ft {
        set_plugin_list_field(doc, url, "on_ft", fts)?;
    }
    if let Some(r) = rev {
        let plugins = doc["plugins"]
            .as_array_of_tables_mut()
            .context("plugins is not an array of tables")?;
        let plugin_table = plugins
            .iter_mut()
            .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
            .context("Could not find plugin in toml_edit document")?;
        plugin_table["rev"] = value(r);
    }
    Ok(())
}

/// `<config_root>/before.lua` / `after.lua` を検出して LoaderOptions を構築する。
fn build_loader_options(config_root: &Path) -> crate::loader::LoaderOptions {
    crate::loader::LoaderOptions {
        global_before: find_lua(config_root, "before.lua"),
        global_after: find_lua(config_root, "after.lua"),
        profile: None,
    }
}

fn write_loader_to_path(
    merged_dir: &Path,
    scripts: &[crate::loader::PluginScripts],
    loader_path: &Path,
    loader_opts: &crate::loader::LoaderOptions,
) -> Result<()> {
    if let Some(parent) = loader_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lua = generate_loader(merged_dir, scripts, loader_opts);
    std::fs::write(loader_path, lua)?;
    Ok(())
}

/// デフォルト並列数。GitHub の rate limit を避けるため控えめに。
const DEFAULT_CONCURRENCY: usize = 13;

fn resolve_concurrency(config_value: Option<usize>) -> usize {
    config_value.unwrap_or(DEFAULT_CONCURRENCY)
}

pub(crate) fn plural_s(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

// ====================================================================
// rvpm init: Neovim init.lua に loader をつなぐためのヘルパー
// ====================================================================

/// `$NVIM_APPNAME` を考慮して init.lua のパスを返す (pure function、テスト容易性のため env は外から注入)。
fn nvim_init_lua_path_for_appname(appname: Option<&str>) -> PathBuf {
    let appname = appname.unwrap_or("nvim");
    let home = dirs::home_dir().expect("Could not find home directory");
    home.join(".config").join(appname).join("init.lua")
}

/// 実行時の `$NVIM_APPNAME` 環境変数を見て init.lua のパスを返す。
fn nvim_init_lua_path() -> PathBuf {
    let appname = std::env::var("NVIM_APPNAME").ok();
    nvim_init_lua_path_for_appname(appname.as_deref())
}

/// loader.lua を参照する `dofile(...)` 行を config から生成する。
/// 優先順位: `options.cache_root`/plugins/loader.lua > `~/.cache/rvpm/<appname>/plugins/loader.lua`
/// tilde 形式を保持することで dotfiles のマシン間共有を妨げない。
fn loader_init_snippet(config: &config::Config) -> String {
    let raw_path = if let Some(base) = &config.options.cache_root {
        format!("{}/plugins/loader.lua", base.trim_end_matches(['/', '\\']))
    } else {
        format!("~/.cache/rvpm/{}/plugins/loader.lua", appname())
    };
    // Windows のバックスラッシュを Lua 文字列リテラルで安全な '/' に正規化。
    let raw_path = raw_path.replace('\\', "/");
    format!("dofile(vim.fn.expand(\"{}\"))", raw_path)
}

/// init.lua が rvpm の loader を参照しているかを緩く検出する。
/// 同じ行内に `rvpm` と `loader.lua` が両方出ていれば真。
pub(crate) fn init_lua_references_rvpm_loader(init_lua_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(init_lua_path) else {
        return false;
    };
    content
        .lines()
        .any(|line| line.contains("rvpm") && line.contains("loader.lua"))
}

#[derive(Debug, PartialEq, Eq)]
enum WriteInitResult {
    /// init.lua が存在しなかったので新規作成した
    Created,
    /// 既存 init.lua に末尾追記した
    Appended,
    /// 既に loader を参照していて変更不要だった
    AlreadyConfigured,
}

/// init.lua に loader snippet を書き込む (冪等)。
fn write_init_lua_snippet(init_lua_path: &Path, snippet: &str) -> Result<WriteInitResult> {
    if init_lua_path.exists() {
        if init_lua_references_rvpm_loader(init_lua_path) {
            return Ok(WriteInitResult::AlreadyConfigured);
        }
        let mut content = std::fs::read_to_string(init_lua_path)?;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("\n-- rvpm loader (auto-added by `rvpm init --write`)\n");
        content.push_str(snippet);
        content.push('\n');
        std::fs::write(init_lua_path, content)?;
        Ok(WriteInitResult::Appended)
    } else {
        if let Some(parent) = init_lua_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = format!(
            "-- Neovim config (auto-created by `rvpm init --write`)\n\n-- rvpm loader\n{}\n",
            snippet
        );
        std::fs::write(init_lua_path, content)?;
        Ok(WriteInitResult::Created)
    }
}

/// `rvpm sync` / `rvpm generate` / `rvpm add` 等の末尾で呼ぶ hint 表示。
/// init.lua が loader を参照していない (or 未作成) なら案内を出す。
fn print_init_lua_hint_if_missing(config: &config::Config) {
    let init_lua_path = nvim_init_lua_path();
    if !init_lua_path.exists() {
        println!();
        println!(
            "\u{26a0} Neovim init.lua not found at {}",
            init_lua_path.display()
        );
        println!("  Run `rvpm init --write` to create one with the rvpm loader.");
        return;
    }
    if !init_lua_references_rvpm_loader(&init_lua_path) {
        let snippet = loader_init_snippet(config);
        println!();
        println!(
            "\u{26a0} {} doesn't reference rvpm loader yet.",
            init_lua_path.display()
        );
        println!("  Add this line:");
        println!("    {}", snippet);
        println!("  Or run `rvpm init --write` to do it automatically.");
    }
}

/// config.toml 上で指定プラグイン (url 一致) の `url = "..."` 行の行番号 (1-indexed) を返す。
/// 見つからなければ 1 を返す (ファイル先頭)。
/// whitespace の入り方に寛容: `url="..."`, `url = "..."`, `url  =   "..."` など全部拾う。
fn find_plugin_line_in_toml(toml_content: &str, url: &str) -> usize {
    let needle = format!("\"{}\"", url);
    for (i, line) in toml_content.lines().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("url") {
            continue;
        }
        // "url" の後は空白 or "=" しか来ないはず (他のフィールド名は "url..." で始まらない)
        let rest = trimmed["url".len()..].trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        if line.contains(&needle) {
            return i + 1;
        }
    }
    1
}

/// `$EDITOR` が `+<line>` 形式の行ジャンプをサポートするか簡易判定。
/// nvim/vim/vi/nano/emacs ファミリーは真。VS Code / helix 等は偽。
fn editor_supports_line_jump(editor_cmd: &str) -> bool {
    // Unix の Path は `\` をパス区切りと認識しないため、手動で両方で split する
    let file_name = editor_cmd.rsplit(['/', '\\']).next().unwrap_or(editor_cmd);
    let base = file_name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(file_name)
        .to_lowercase();
    matches!(base.as_str(), "nvim" | "vim" | "vi" | "nano" | "emacs")
}

/// `$EDITOR` (未設定なら "nvim") でファイルを開く。対応している editor なら指定行にジャンプ。
fn open_editor_at_line(path: &Path, line: usize) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
    let mut cmd = std::process::Command::new(&editor);
    if editor_supports_line_jump(&editor) {
        cmd.arg(format!("+{}", line));
    }
    cmd.arg(path);
    cmd.status()?;
    Ok(())
}

pub(crate) fn find_lua(dir: &Path, name: &str) -> Option<String> {
    let path = dir.join(name);
    if path.exists() {
        Some(path.to_string_lossy().to_string())
    } else {
        None
    }
}

/// loader.lua を一時的に差し替える間、panic / 非正常終了でも原本を戻す Drop guard。
///
/// `rvpm profile` は計測のため profile_mode=true で再生成した loader.lua を
/// 現役パスに上書きし、終了時に元の内容へ戻す。途中で rvpm が落ちても、次回の
/// sync / generate / profile が marker 残骸や不正な状態にならないよう、Drop で
/// 原本復元を試みる。
struct LoaderSwapGuard {
    loader_path: PathBuf,
    backup_path: PathBuf,
    committed: bool,
}

impl LoaderSwapGuard {
    fn create(loader_path: PathBuf) -> Result<Self> {
        let backup_path = loader_path.with_extension("lua.bak");
        if backup_path.exists() {
            let _ = std::fs::remove_file(&backup_path);
        }
        if loader_path.exists() {
            std::fs::rename(&loader_path, &backup_path).with_context(|| {
                format!("failed to back up loader.lua to {}", backup_path.display())
            })?;
        }
        Ok(Self {
            loader_path,
            backup_path,
            committed: false,
        })
    }

    fn commit(mut self) -> Result<()> {
        self.restore()?;
        self.committed = true;
        Ok(())
    }

    fn restore(&self) -> Result<()> {
        if !self.backup_path.exists() {
            return Ok(());
        }
        if self.loader_path.exists() {
            let _ = std::fs::remove_file(&self.loader_path);
        }
        std::fs::rename(&self.backup_path, &self.loader_path).with_context(|| {
            format!(
                "failed to restore loader.lua from {}",
                self.backup_path.display()
            )
        })
    }
}

impl Drop for LoaderSwapGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Err(e) = self.restore() {
            eprintln!(
                "\u{26a0} rvpm profile: failed to auto-restore loader.lua on drop: {} — run `rvpm generate` to rebuild",
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Tests for code that still lives in `lib.rs`. Tests for the extracted
    // `paths` / `merge` / `url` / `plugin_build` modules were relocated next to
    // those modules in #224; the `url` glob stays for the few lib.rs tests that
    // still call into it.
    use crate::config::{Config, MapSpec, Options, Plugin};
    use crate::loader::PluginScripts;
    use crate::url::*;
    use tempfile::tempdir;
    use toml_edit::DocumentMut;

    // ── ensure_absent: 残骸混入を防ぐ事前 cleanup ────────────────────

    #[test]
    fn patch_plugin_entry_triggers_does_not_overwrite_existing_on_cmd() {
        // user が `--on-cmd MyCmd` で明示指定した / 手編集で既に書いた entry は
        // scan 結果で上書きしてはいけない (PR #91 review 指摘)。
        let initial = r#"[[plugins]]
url = "owner/repo"
on_cmd = ["MyCmd"]
"#;
        let mut doc = initial.parse::<DocumentMut>().unwrap();
        let applied = AddTriggerSuggestion {
            on_cmd: vec!["ScannedFoo".into(), "ScannedBar".into()],
            on_map: vec![],
        };
        patch_plugin_entry_triggers(&mut doc, "owner/repo", &applied);
        let out = doc.to_string();
        assert!(
            out.contains(r#"on_cmd = ["MyCmd"]"#),
            "existing on_cmd must be preserved, got:\n{out}"
        );
        assert!(
            !out.contains("ScannedFoo"),
            "scan result must not be written"
        );
    }

    #[test]
    fn patch_plugin_entry_triggers_does_not_overwrite_existing_on_map() {
        let initial = r#"[[plugins]]
url = "owner/repo"
on_map = [{lhs = "<leader>x", mode = ["n", "x"], desc = "custom"}]
"#;
        let mut doc = initial.parse::<DocumentMut>().unwrap();
        let applied = AddTriggerSuggestion {
            on_cmd: vec![],
            on_map: vec![MapSpec {
                lhs: "gc".into(),
                mode: vec!["n".into()],
                desc: None,
            }],
        };
        patch_plugin_entry_triggers(&mut doc, "owner/repo", &applied);
        let out = doc.to_string();
        assert!(
            out.contains("<leader>x"),
            "existing on_map must be preserved:\n{out}"
        );
        assert!(!out.contains("gc"), "scan result must not be written");
    }

    #[test]
    fn patch_plugin_entry_triggers_writes_when_field_absent() {
        // 既存 entry に on_cmd が無ければ scan 結果を書く (通常パス)。
        let initial = r#"[[plugins]]
url = "owner/repo"
"#;
        let mut doc = initial.parse::<DocumentMut>().unwrap();
        let applied = AddTriggerSuggestion {
            on_cmd: vec!["ScannedFoo".into()],
            on_map: vec![],
        };
        patch_plugin_entry_triggers(&mut doc, "owner/repo", &applied);
        let out = doc.to_string();
        assert!(out.contains("ScannedFoo"));
    }

    // ─── replace_plugin_entry_with_ai_toml (#95) ─────────────────────────

    #[test]
    fn replace_plugin_entry_with_ai_toml_preserves_position_among_other_entries() {
        // [vars] や既存 [[plugins]] の中に stub が並んでいるとき、in-place マージが
        // entry の位置を維持することを確認 (user 報告: AI 提案が `[vars]` 直後に
        // 飛んで配置される現象の回帰 test)。
        let initial = r#"[vars]
nvim_rc = "~/.config/nvim/rc"

[[plugins]]
url = "first/plugin"
on_cmd = ["First"]

[[plugins]]
url = "second/stub"

[[plugins]]
url = "third/plugin"
"#;
        let mut doc = initial.parse::<DocumentMut>().unwrap();
        let proposal = r#"[[plugins]]
url = "second/stub"
on_event = ["BufReadPre"]
"#;
        replace_plugin_entry_with_ai_toml(&mut doc, "second/stub", proposal, &[], MergeMode::Merge)
            .unwrap();
        let out = doc.to_string();
        // first/plugin → second/stub → third/plugin の順序が維持される
        let first_pos = out.find("first/plugin").unwrap();
        let second_pos = out.find("second/stub").unwrap();
        let third_pos = out.find("third/plugin").unwrap();
        assert!(first_pos < second_pos);
        assert!(second_pos < third_pos);
        assert!(out.contains(r#"on_event = ["BufReadPre"]"#));
    }

    #[test]
    fn replace_plugin_entry_with_ai_toml_preserves_user_specified_keys() {
        // user の `--rev` / `--on-cmd` が既に stub に書かれている場合、AI 提案で
        // 上書きしないことを確認 (CodeRabbit Major #95)。
        let initial = r#"[[plugins]]
url = "owner/repo"
rev = "stable"
on_cmd = ["ManualCmd"]
"#;
        let mut doc = initial.parse::<DocumentMut>().unwrap();
        let proposal = r#"[[plugins]]
url = "owner/repo"
rev = "main"
on_cmd = ["AICmd"]
on_map = ["<leader>x"]
"#;
        // user が --rev と --on-cmd を明示したシナリオ
        let preserved = ["url", "rev", "on_cmd"];
        replace_plugin_entry_with_ai_toml(
            &mut doc,
            "owner/repo",
            proposal,
            &preserved,
            MergeMode::Merge,
        )
        .unwrap();
        let out = doc.to_string();
        // user の rev / on_cmd が残る
        assert!(
            out.contains(r#"rev = "stable""#),
            "user --rev must be preserved:\n{out}"
        );
        assert!(
            out.contains(r#"on_cmd = ["ManualCmd"]"#),
            "user --on-cmd must be preserved:\n{out}"
        );
        // AI が新規に追加した on_map は反映される
        assert!(
            out.contains("on_map"),
            "AI-only field must be added:\n{out}"
        );
        // url は stored_url で正規化される
        assert!(out.contains(r#"url = "owner/repo""#));
    }

    #[test]
    fn replace_plugin_entry_with_ai_toml_writes_ai_keys_when_no_preservation() {
        // preserved_keys が空 (user が CLI flag を出してない) なら AI 提案が全反映。
        let initial = r#"[[plugins]]
url = "owner/repo"
"#;
        let mut doc = initial.parse::<DocumentMut>().unwrap();
        let proposal = r#"[[plugins]]
url = "different/url"
on_cmd = ["AICmd"]
"#;
        replace_plugin_entry_with_ai_toml(&mut doc, "owner/repo", proposal, &[], MergeMode::Merge)
            .unwrap();
        let out = doc.to_string();
        // url は stored_url を維持 (AI が url を勘違いしても保護)
        assert!(out.contains(r#"url = "owner/repo""#));
        assert!(!out.contains("different/url"));
        assert!(out.contains(r#"on_cmd = ["AICmd"]"#));
    }

    #[test]
    fn replace_plugin_entry_with_ai_toml_rejects_proposal_with_zero_or_many_entries() {
        let initial = r#"[[plugins]]
url = "x/y"
"#;
        let mut doc = initial.parse::<DocumentMut>().unwrap();

        let zero = r#"name = "no plugins""#;
        assert!(
            replace_plugin_entry_with_ai_toml(&mut doc, "x/y", zero, &[], MergeMode::Merge)
                .is_err()
        );

        let many = r#"[[plugins]]
url = "a/b"

[[plugins]]
url = "c/d"
"#;
        assert!(
            replace_plugin_entry_with_ai_toml(&mut doc, "x/y", many, &[], MergeMode::Merge)
                .is_err()
        );
    }

    #[test]
    fn replace_plugin_entry_with_ai_toml_replace_mode_drops_stale_keys() {
        // CodeRabbit PR #100 指摘: tune で AI が omit した既存 field が
        // (`on_cmd`, `rev` 等) 残ってしまう問題の回帰 test。
        let initial = r#"[[plugins]]
url = "owner/tuneme"
on_cmd = ["StaleCmd"]
rev = "abc123"
on_ft = "rust"
"#;
        let mut doc = initial.parse::<DocumentMut>().unwrap();
        // AI 提案は on_cmd のみ — rev / on_ft は意図的に omit して "削除して" の意図。
        let proposal = r#"[[plugins]]
url = "owner/tuneme"
on_cmd = ["NewCmd"]
"#;
        replace_plugin_entry_with_ai_toml(
            &mut doc,
            "owner/tuneme",
            proposal,
            &[],
            MergeMode::Replace,
        )
        .unwrap();
        let out = doc.to_string();
        assert!(out.contains(r#"on_cmd = ["NewCmd"]"#));
        // 提案に含まれない既存 field は削除される
        assert!(
            !out.contains(r#"rev = "abc123""#),
            "Replace mode must drop stale rev:\n{out}"
        );
        assert!(
            !out.contains(r#"on_ft = "rust""#),
            "Replace mode must drop stale on_ft:\n{out}"
        );
        // url は強制保持
        assert!(out.contains(r#"url = "owner/tuneme""#));
    }

    #[test]
    fn replace_plugin_entry_with_ai_toml_replace_mode_keeps_preserved_keys() {
        // Replace mode でも `preserved_keys` に挙げた field は残す
        // (将来 `tune --keep rev` のような flag を足したときの保証)。
        let initial = r#"[[plugins]]
url = "owner/repo"
rev = "stable"
on_cmd = ["KeepMe"]
"#;
        let mut doc = initial.parse::<DocumentMut>().unwrap();
        let proposal = r#"[[plugins]]
url = "owner/repo"
on_event = ["BufRead"]
"#;
        replace_plugin_entry_with_ai_toml(
            &mut doc,
            "owner/repo",
            proposal,
            &["rev"],
            MergeMode::Replace,
        )
        .unwrap();
        let out = doc.to_string();
        // preserved な rev は残る
        assert!(out.contains(r#"rev = "stable""#));
        // preserved でない on_cmd は AI 提案に無いので消える
        assert!(!out.contains("KeepMe"));
        // AI 提案の on_event は新規追加
        assert!(out.contains(r#"on_event = ["BufRead"]"#));
    }

    // ─── extract_plugin_entry_toml (rvpm tune) ──────────────────────────

    #[test]
    fn extract_plugin_entry_toml_returns_full_block_with_header() {
        let toml = r#"[options]
ai = "claude"

[[plugins]]
url = "owner/first"
on_cmd = ["First"]

[[plugins]]
url = "owner/target"
on_cmd = ["Target"]
on_ft = "rust"

[[plugins]]
url = "owner/last"
"#;
        let doc = toml.parse::<DocumentMut>().unwrap();
        let entry = extract_plugin_entry_toml(&doc, "owner/target").unwrap();
        // header が付く
        assert!(entry.starts_with("[[plugins]]"));
        // 中身を含む
        assert!(entry.contains(r#"url = "owner/target""#));
        assert!(entry.contains(r#"on_cmd = ["Target"]"#));
        assert!(entry.contains(r#"on_ft = "rust""#));
        // 他 entry は含まれない
        assert!(!entry.contains("owner/first"));
        assert!(!entry.contains("owner/last"));
    }

    #[test]
    fn extract_plugin_entry_toml_returns_none_for_missing_url() {
        let toml = r#"[[plugins]]
url = "only/one"
"#;
        let doc = toml.parse::<DocumentMut>().unwrap();
        assert!(extract_plugin_entry_toml(&doc, "missing/repo").is_none());
    }

    #[test]
    fn extract_plugin_entry_toml_returns_none_when_plugins_missing() {
        let toml = "[options]\nai = \"claude\"\n";
        let doc = toml.parse::<DocumentMut>().unwrap();
        assert!(extract_plugin_entry_toml(&doc, "any/url").is_none());
    }

    // ─── select_plugin_url (rvpm tune / set / remove 共通) ───────────────

    #[test]
    fn select_plugin_url_query_exact_match_returns_url() {
        use crate::config::Plugin;
        let plugins = vec![
            Plugin {
                url: "owner/first".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "owner/second".to_string(),
                ..Default::default()
            },
        ];
        let got = select_plugin_url(&plugins, Some("owner/second"), "select").unwrap();
        assert_eq!(got, Some("owner/second".to_string()));
    }

    #[test]
    fn select_plugin_url_query_substring_match_returns_first_match() {
        use crate::config::Plugin;
        let plugins = vec![
            Plugin {
                url: "alpha/foo".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "beta/bar".to_string(),
                ..Default::default()
            },
        ];
        let got = select_plugin_url(&plugins, Some("beta"), "select").unwrap();
        assert_eq!(got, Some("beta/bar".to_string()));
    }

    #[test]
    fn select_plugin_url_query_no_match_errors() {
        use crate::config::Plugin;
        let plugins = vec![Plugin {
            url: "owner/repo".to_string(),
            ..Default::default()
        }];
        let err = select_plugin_url(&plugins, Some("nonexistent"), "select").unwrap_err();
        assert!(err.to_string().contains("Plugin not found"));
    }

    #[test]
    fn select_plugin_url_exact_match_wins_over_partial() {
        // user typed `cmp` and a plugin called exactly `cmp` exists alongside
        // longer `cmp-buffer` etc → exact match takes precedence (no ambiguity).
        use crate::config::Plugin;
        let plugins = vec![
            Plugin {
                url: "hrsh7th/cmp-buffer".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "cmp".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "hrsh7th/cmp-cmdline".to_string(),
                ..Default::default()
            },
        ];
        let got = select_plugin_url(&plugins, Some("cmp"), "select").unwrap();
        assert_eq!(got, Some("cmp".to_string()));
    }

    #[test]
    fn select_plugin_url_ambiguous_partial_errors_with_listing() {
        // CodeRabbit PR #100 指摘: 複数の partial match を黙って先頭採用すると
        // mutating コマンドが意図しない plugin を変更してしまう。複数 match は
        // error にし、候補一覧を見せる。
        use crate::config::Plugin;
        let plugins = vec![
            Plugin {
                url: "hrsh7th/cmp-buffer".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "hrsh7th/cmp-cmdline".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "hrsh7th/cmp-path".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "folke/snacks.nvim".to_string(),
                ..Default::default()
            },
        ];
        let err = select_plugin_url(&plugins, Some("cmp"), "select").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("matches multiple"));
        // 全候補が表示される
        assert!(msg.contains("hrsh7th/cmp-buffer"));
        assert!(msg.contains("hrsh7th/cmp-cmdline"));
        assert!(msg.contains("hrsh7th/cmp-path"));
        // match しないものは含まれない
        assert!(!msg.contains("snacks"));
    }

    // ─── classify_held_back ──────────────────────────────────────────────
    // Pure classification of "this plugin is being held back by a lockfile
    // pin". The integration path is covered by the git::remote_head test
    // on the git.rs side; these tests nail down the decision table only.

    #[test]
    fn test_classify_held_back_reports_pin_behind_remote() {
        // No rev set, lockfile contributed a commit, and HEAD (= pin)
        // lags behind remote → this is the case users hit.
        let got = classify_held_back(
            None,
            Some("lockcommit"),
            Some("lockcommit"),
            Some("newcommit"),
        );
        assert_eq!(
            got,
            Some(("lockcommit".to_string(), "newcommit".to_string()))
        );
    }

    #[test]
    fn test_classify_held_back_silent_when_explicit_rev_set() {
        // If the user pinned via `rev`, the lag is intentional — not our
        // call to flag.
        let got = classify_held_back(Some("v1.0.0"), Some("v1.0.0"), Some("abc"), Some("def"));
        assert_eq!(got, None);
    }

    #[test]
    fn test_classify_held_back_silent_without_lockfile_contribution() {
        // No lockfile entry → sync already reset to remote; there's no pin
        // holding us back.
        let got = classify_held_back(None, None, Some("abc"), Some("abc"));
        assert_eq!(got, None);
    }

    #[test]
    fn test_classify_held_back_silent_when_pin_matches_remote() {
        // Lockfile pin and remote happen to line up — everyone's up to
        // date, no reason to nag.
        let got = classify_held_back(None, Some("abc"), Some("abc"), Some("abc"));
        assert_eq!(got, None);
    }

    #[test]
    fn test_classify_held_back_silent_when_remote_head_unknown() {
        // Can't prove the pin is behind if we can't resolve the remote tip.
        // Prefer silence over a false positive (resilience).
        let got = classify_held_back(None, Some("abc"), Some("abc"), None);
        assert_eq!(got, None);
    }

    #[test]
    fn test_classify_held_back_silent_when_head_unknown() {
        // Same resilience argument in the other direction.
        let got = classify_held_back(None, Some("abc"), None, Some("def"));
        assert_eq!(got, None);
    }

    #[test]
    fn test_resolve_plugin_dst_expands_tilde_in_custom_dst() {
        // `dst = "~/src/foo"` のような tilde 付きパスは home dir に展開される。
        // 展開されないと dev プラグインの exists() チェックが常に false になる。
        let home = dirs::home_dir().expect("home dir");
        let cache_root = PathBuf::from("/tmp/rvpm-cache");
        let plugin = Plugin {
            url: "yukimemi/snacks-source-chronicle".to_string(),
            dst: Some("~/src/github.com/yukimemi/snacks-source-chronicle".to_string()),
            dev: true,
            ..Default::default()
        };
        let got = resolve_plugin_dst(&plugin, &cache_root);
        assert_eq!(
            got,
            home.join("src/github.com/yukimemi/snacks-source-chronicle")
        );
    }

    #[test]
    fn test_resolve_plugin_dst_uses_cache_root_when_dst_unset() {
        let cache_root = PathBuf::from("/tmp/rvpm-cache");
        let plugin = Plugin {
            url: "folke/snacks.nvim".to_string(),
            ..Default::default()
        };
        let got = resolve_plugin_dst(&plugin, &cache_root);
        // repos_dir は `{cache_root}/plugins/repos`
        assert!(got.starts_with(cache_root.join("plugins/repos")));
    }

    // ─── matches_rebuild_filter ─────────────────────────────────────────
    // `--rebuild [QUERY]` のスコープ判定を 3 分岐で押さえる:
    //   None        → 常に false (フラグ未指定、従来デフォルト)
    //   Some("")    → 常に true (フラグだけ、従来の `--rebuild` 挙動)
    //   Some("q")   → url / name に q を含めば true

    fn mk_plugin(url: &str, name: Option<&str>) -> Plugin {
        Plugin {
            url: url.to_string(),
            name: name.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_rebuild_filter_none_never_matches() {
        let p = mk_plugin("nvim-treesitter/nvim-treesitter", None);
        assert!(!matches_rebuild_filter(&p, None));
    }

    #[test]
    fn test_rebuild_filter_empty_always_matches() {
        let p = mk_plugin("folke/snacks.nvim", None);
        assert!(matches_rebuild_filter(&p, Some("")));
    }

    #[test]
    fn test_rebuild_filter_substring_matches_url() {
        // 呼び出し側で lowercase 済みの query を渡す契約。
        let p = mk_plugin("nvim-treesitter/nvim-treesitter", None);
        assert!(matches_rebuild_filter(&p, Some("treesitter")));
        assert!(!matches_rebuild_filter(&p, Some("telescope")));
    }

    #[test]
    fn test_rebuild_filter_requires_caller_to_lowercase_query() {
        // 契約: caller が lowercase してから渡す。大文字混じりは一致しない
        // (run_sync 側で事前正規化する理由)。
        let p = mk_plugin("nvim-treesitter/nvim-treesitter", None);
        assert!(
            !matches_rebuild_filter(&p, Some("TREESITTER")),
            "case-insensitivity is the caller's responsibility"
        );
    }

    #[test]
    fn test_rebuild_filter_matches_explicit_name() {
        // URL と name が独立: name 側で hit させたいケース
        let p = mk_plugin("owner/repo", Some("my-alias"));
        assert!(matches_rebuild_filter(&p, Some("alias")));
    }

    #[test]
    fn test_rebuild_filter_no_false_match_without_name() {
        // name = None のとき、url にだけマッチングが走る (name 側で空文字との
        // 意図せぬ contains が起きないこと)
        let p = mk_plugin("foo/bar", None);
        assert!(!matches_rebuild_filter(&p, Some("baz")));
    }

    #[test]
    fn test_parse_config_url_style_defaults_to_short() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
"#;
        let config = crate::config::parse_config(toml).unwrap();
        assert_eq!(config.options.url_style, crate::config::UrlStyle::Short);
    }

    #[test]
    fn test_parse_config_accepts_url_style_full() {
        let toml = r#"
[options]
url_style = "full"

[[plugins]]
url = "owner/repo"
"#;
        let config = crate::config::parse_config(toml).unwrap();
        assert_eq!(config.options.url_style, crate::config::UrlStyle::Full);
    }

    #[test]
    fn test_update_filters_by_query() {
        let plugins = [
            Plugin {
                url: "owner/telescope.nvim".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "owner/plenary.nvim".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "owner/nvim-cmp".to_string(),
                ..Default::default()
            },
        ];
        let query = Some("telescope".to_string());
        let filtered: Vec<_> = plugins
            .iter()
            .filter(|p| {
                if let Some(q) = &query {
                    p.url.contains(q.as_str())
                } else {
                    true
                }
            })
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].url, "owner/telescope.nvim");
    }

    #[test]
    fn test_update_no_query_matches_all() {
        let plugins = [
            Plugin {
                url: "owner/telescope.nvim".to_string(),
                ..Default::default()
            },
            Plugin {
                url: "owner/plenary.nvim".to_string(),
                ..Default::default()
            },
        ];
        let query: Option<String> = None;
        let filtered: Vec<_> = plugins
            .iter()
            .filter(|p| {
                if let Some(q) = &query {
                    p.url.contains(q.as_str())
                } else {
                    true
                }
            })
            .collect();
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_nvim_init_lua_path_for_appname_defaults_to_nvim() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            nvim_init_lua_path_for_appname(None),
            home.join(".config").join("nvim").join("init.lua")
        );
    }

    #[test]
    fn test_nvim_init_lua_path_for_appname_respects_nvim_appname() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            nvim_init_lua_path_for_appname(Some("mynvim")),
            home.join(".config").join("mynvim").join("init.lua")
        );
    }

    #[test]
    fn test_loader_init_snippet_uses_default_when_no_options() {
        let cfg = config::Config {
            vars: None,
            options: config::Options::default(),
            plugins: vec![],
        };
        let snippet = loader_init_snippet(&cfg);
        // appname は env 依存なので partial match
        assert!(snippet.starts_with("dofile(vim.fn.expand(\"~/.cache/rvpm/"));
        assert!(snippet.ends_with("/plugins/loader.lua\"))"));
    }

    #[test]
    fn test_loader_init_snippet_uses_cache_root_when_set() {
        let cfg = config::Config {
            vars: None,
            options: config::Options {
                cache_root: Some("~/dotfiles/rvpm".to_string()),
                ..Default::default()
            },
            plugins: vec![],
        };
        assert_eq!(
            loader_init_snippet(&cfg),
            "dofile(vim.fn.expand(\"~/dotfiles/rvpm/plugins/loader.lua\"))"
        );
    }

    #[test]
    fn test_loader_init_snippet_normalizes_windows_path_separators() {
        let cfg = config::Config {
            vars: None,
            options: config::Options {
                cache_root: Some(r"C:\Users\test\.cache\rvpm\nvim".to_string()),
                ..Default::default()
            },
            plugins: vec![],
        };
        let snippet = loader_init_snippet(&cfg);
        assert!(
            !snippet.contains('\\'),
            "snippet contains backslash: {snippet}"
        );
        assert_eq!(
            snippet,
            "dofile(vim.fn.expand(\"C:/Users/test/.cache/rvpm/nvim/plugins/loader.lua\"))"
        );
    }

    #[test]
    fn test_loader_init_snippet_trims_trailing_backslash() {
        let cfg = config::Config {
            vars: None,
            options: config::Options {
                cache_root: Some(r"C:\cache\rvpm\".to_string()),
                ..Default::default()
            },
            plugins: vec![],
        };
        assert_eq!(
            loader_init_snippet(&cfg),
            "dofile(vim.fn.expand(\"C:/cache/rvpm/plugins/loader.lua\"))"
        );
    }

    #[test]
    fn test_init_lua_references_rvpm_loader_detects_line() {
        let root = tempdir().unwrap();
        let path = root.path().join("init.lua");
        std::fs::write(
            &path,
            "-- some\ndofile(vim.fn.expand(\"~/.cache/rvpm/loader.lua\"))\n",
        )
        .unwrap();
        assert!(init_lua_references_rvpm_loader(&path));
    }

    #[test]
    fn test_init_lua_references_rvpm_loader_false_when_absent() {
        let root = tempdir().unwrap();
        let path = root.path().join("init.lua");
        std::fs::write(&path, "-- empty\nvim.g.mapleader = ' '\n").unwrap();
        assert!(!init_lua_references_rvpm_loader(&path));
    }

    #[test]
    fn test_init_lua_references_rvpm_loader_false_when_file_missing() {
        let root = tempdir().unwrap();
        let path = root.path().join("missing.lua");
        assert!(!init_lua_references_rvpm_loader(&path));
    }

    #[test]
    fn test_init_lua_references_rvpm_loader_requires_both_keywords() {
        let root = tempdir().unwrap();
        let path = root.path().join("init.lua");
        // "loader.lua" だけでは rvpm の loader 参照と判定しない
        std::fs::write(&path, "dofile(\"~/other/loader.lua\")\n").unwrap();
        assert!(!init_lua_references_rvpm_loader(&path));
    }

    #[test]
    fn test_write_init_lua_snippet_creates_when_missing() {
        let root = tempdir().unwrap();
        let init_path = root.path().join("nvim").join("init.lua");
        let snippet = "dofile(vim.fn.expand(\"~/.cache/rvpm/loader.lua\"))";
        let result = write_init_lua_snippet(&init_path, snippet).unwrap();
        assert!(matches!(result, WriteInitResult::Created));
        assert!(init_path.exists());
        let content = std::fs::read_to_string(&init_path).unwrap();
        assert!(content.contains(snippet));
        assert!(content.contains("rvpm"));
    }

    #[test]
    fn test_write_init_lua_snippet_appends_when_exists_without_loader() {
        let root = tempdir().unwrap();
        let init_path = root.path().join("init.lua");
        std::fs::write(&init_path, "-- existing\nvim.g.mapleader = ' '\n").unwrap();
        let snippet = "dofile(vim.fn.expand(\"~/.cache/rvpm/loader.lua\"))";
        let result = write_init_lua_snippet(&init_path, snippet).unwrap();
        assert!(matches!(result, WriteInitResult::Appended));
        let content = std::fs::read_to_string(&init_path).unwrap();
        assert!(content.contains("mapleader"));
        assert!(content.contains(snippet));
    }

    #[test]
    fn test_write_init_lua_snippet_noop_when_already_configured() {
        let root = tempdir().unwrap();
        let init_path = root.path().join("init.lua");
        std::fs::write(
            &init_path,
            "dofile(vim.fn.expand(\"~/.cache/rvpm/loader.lua\"))\n",
        )
        .unwrap();
        let result = write_init_lua_snippet(
            &init_path,
            "dofile(vim.fn.expand(\"~/.cache/rvpm/loader.lua\"))",
        )
        .unwrap();
        assert!(matches!(result, WriteInitResult::AlreadyConfigured));
        let content = std::fs::read_to_string(&init_path).unwrap();
        // 行数が増えていないこと
        assert_eq!(content.lines().count(), 1);
    }

    #[test]
    fn test_write_loader_to_path_creates_file() {
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");
        std::fs::create_dir_all(&merged).unwrap();
        let loader_path = root.path().join("custom").join("loader.lua");
        let scripts: Vec<PluginScripts> = vec![];
        write_loader_to_path(
            &merged,
            &scripts,
            &loader_path,
            &crate::loader::LoaderOptions::default(),
        )
        .unwrap();
        assert!(loader_path.exists());
        let content = std::fs::read_to_string(&loader_path).unwrap();
        assert!(content.contains("-- rvpm generated loader.lua"));
    }

    #[test]
    fn test_resolve_concurrency_defaults_to_13() {
        let result = resolve_concurrency(None);
        assert_eq!(result, DEFAULT_CONCURRENCY);
        assert_eq!(result, 13);
    }

    fn mk_test_plugin() -> crate::config::Plugin {
        // toml::from_str で必須 default を埋めた素の Plugin を作る (テスト用)
        toml::from_str::<crate::config::Plugin>(r#"url = "owner/repo""#).unwrap()
    }

    #[test]
    fn test_disable_merge_if_cond_no_cond_passthrough() {
        let mut p = mk_test_plugin();
        p.merge = true;
        p.merge_doc = Some(true);
        disable_merge_if_cond(&mut p);
        assert!(p.merge);
        assert_eq!(p.merge_doc, Some(true));
    }

    #[test]
    fn test_disable_merge_if_cond_forces_merge_false() {
        let mut p = mk_test_plugin();
        p.cond = Some("vim.fn.has('win32') == 1".to_string());
        p.merge = true;
        disable_merge_if_cond(&mut p);
        assert!(!p.merge);
    }

    #[test]
    fn test_disable_merge_if_cond_explicit_per_plugin_merge_doc_survives() {
        // per-plugin Some(true) は cond でも尊重 (Windows 限定 plugin の help を
        // クロスプラットフォームで引きたいケース)
        let mut p = mk_test_plugin();
        p.cond = Some("vim.fn.has('win32') == 1".to_string());
        p.merge_doc = Some(true);
        disable_merge_if_cond(&mut p);
        assert_eq!(p.merge_doc, Some(true));
    }

    #[test]
    fn test_disable_merge_if_cond_unset_merge_doc_forced_false() {
        // per-plugin 未指定 (None) は global default を継ぐので、
        // cond が立っているなら sweep を防ぐため Some(false) に固定する。
        let mut p = mk_test_plugin();
        p.cond = Some("vim.fn.has('win32') == 1".to_string());
        p.merge_doc = None;
        disable_merge_if_cond(&mut p);
        assert_eq!(p.merge_doc, Some(false));
    }

    #[test]
    fn test_disable_merge_if_cond_explicit_false_unchanged() {
        let mut p = mk_test_plugin();
        p.cond = Some("false".to_string());
        p.merge_doc = Some(false);
        disable_merge_if_cond(&mut p);
        assert_eq!(p.merge_doc, Some(false));
    }

    #[test]
    fn test_resolve_concurrency_uses_config_value() {
        let result = resolve_concurrency(Some(5));
        assert_eq!(result, 5);
    }

    #[test]
    fn test_remove_from_toml() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n\n[[plugins]]\nurl = \"owner/b\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        remove_plugin_from_toml(&mut doc, "owner/a").unwrap();
        let result = doc.to_string();
        assert!(!result.contains("owner/a"));
        assert!(result.contains("owner/b"));
    }

    #[test]
    fn test_find_plugin_line_in_toml_basic() {
        let toml = "[options]\n\n[[plugins]]\nurl = \"owner/a\"\nlazy = true\n\n[[plugins]]\nurl = \"owner/b\"\n";
        //            1         2  3             4             5           6  7             8
        assert_eq!(find_plugin_line_in_toml(toml, "owner/a"), 4);
        assert_eq!(find_plugin_line_in_toml(toml, "owner/b"), 8);
    }

    #[test]
    fn test_find_plugin_line_in_toml_handles_whitespace_variants() {
        let toml = "[[plugins]]\nurl=\"owner/a\"\n\n[[plugins]]\nurl  =   \"owner/b\"\n";
        assert_eq!(find_plugin_line_in_toml(toml, "owner/a"), 2);
        assert_eq!(find_plugin_line_in_toml(toml, "owner/b"), 5);
    }

    #[test]
    fn test_find_plugin_line_in_toml_missing_falls_back_to_one() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        assert_eq!(find_plugin_line_in_toml(toml, "owner/nonexistent"), 1);
    }

    #[test]
    fn test_find_plugin_line_in_toml_ignores_substring_matches() {
        // "owner/ab" should not be matched when searching for "owner/a"
        let toml = "[[plugins]]\nurl = \"owner/ab\"\n\n[[plugins]]\nurl = \"owner/a\"\n";
        assert_eq!(find_plugin_line_in_toml(toml, "owner/a"), 5);
    }

    #[test]
    fn test_editor_supports_line_jump() {
        assert!(editor_supports_line_jump("nvim"));
        assert!(editor_supports_line_jump("vim"));
        assert!(editor_supports_line_jump("vi"));
        assert!(editor_supports_line_jump("nano"));
        assert!(editor_supports_line_jump("emacs"));
        assert!(editor_supports_line_jump("/usr/local/bin/nvim"));
        assert!(editor_supports_line_jump(
            "C:\\Program Files\\Neovim\\bin\\nvim.exe"
        ));
        assert!(!editor_supports_line_jump("code"));
        assert!(!editor_supports_line_jump("hx"));
    }

    #[test]
    fn test_remove_from_toml_not_found_returns_error() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        assert!(remove_plugin_from_toml(&mut doc, "owner/nonexistent").is_err());
    }

    #[test]
    fn test_set_plugin_list_field_single_writes_as_string() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        set_plugin_list_field(&mut doc, "owner/a", "on_cmd", vec!["Telescope".to_string()])
            .unwrap();
        let result = doc.to_string();
        assert!(
            result.contains("on_cmd = \"Telescope\""),
            "1要素は文字列として書かれるべき: {}",
            result
        );
        assert!(
            !result.contains("on_cmd = ["),
            "1要素は配列にしないべき: {}",
            result
        );
    }

    #[test]
    fn test_set_plugin_list_field_errors_on_url_mismatch() {
        // `options.url_style = "full"` のケースで run_add 旧コードが踏んでいた
        // regression の回帰テスト。
        //
        // `format_plugin_url("owner/repo", Full)` は `https://github.com/owner/repo`
        // を返し、config.toml にはその canonical URL が書き込まれる。続けて
        // 旧 run_add は入力文字列 (`owner/repo`) をキーに `set_plugin_list_field`
        // を呼んでいたため、entry が見つからず Err を返し、on_cmd の書き込みと
        // 後続の clone が両方失敗していた。現在は `stored_url` をキーに使う。
        //
        // format_plugin_url → set_plugin_list_field の end-to-end contract を
        // 壊さないため、canonical URL をハードコードせず format_plugin_url の
        // 戻り値をそのまま流して両者の一貫性も担保する。
        use crate::config::UrlStyle;
        let input = "owner/repo";
        let stored_url = format_plugin_url(input, UrlStyle::Full);
        assert_eq!(stored_url, "https://github.com/owner/repo");

        let toml = format!("[[plugins]]\nurl = \"{}\"\n", stored_url);

        // 旧バグ: 入力文字列をそのままキーに渡すと entry が見つからない
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        let err = set_plugin_list_field(&mut doc, input, "on_cmd", vec!["Telescope".to_string()]);
        assert!(
            err.is_err(),
            "入力 URL (owner/repo) と entry URL (https://...) が違えば見つからない"
        );

        // 現行: format_plugin_url の戻り値 (stored_url) をそのまま使えば成功
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        set_plugin_list_field(
            &mut doc,
            &stored_url,
            "on_cmd",
            vec!["Telescope".to_string()],
        )
        .unwrap();
        assert!(doc.to_string().contains("on_cmd = \"Telescope\""));
    }

    // -----------------------------------------------------------------
    // --on-* CLI パーサ (Vec<String> 用) のテスト
    // -----------------------------------------------------------------

    #[test]
    fn test_parse_cli_string_list_single_value() {
        let items = parse_cli_string_list("BufReadPre").unwrap();
        assert_eq!(items, vec!["BufReadPre".to_string()]);
    }

    #[test]
    fn test_parse_cli_string_list_comma_separated() {
        let items = parse_cli_string_list("BufReadPre, BufNewFile ,InsertEnter").unwrap();
        assert_eq!(
            items,
            vec![
                "BufReadPre".to_string(),
                "BufNewFile".to_string(),
                "InsertEnter".to_string(),
            ]
        );
    }

    #[test]
    fn test_parse_cli_string_list_json_array() {
        let items = parse_cli_string_list(r#"["BufReadPre", "BufNewFile"]"#).unwrap();
        assert_eq!(
            items,
            vec!["BufReadPre".to_string(), "BufNewFile".to_string()]
        );
    }

    #[test]
    fn test_parse_cli_string_list_json_array_with_user_event() {
        let items = parse_cli_string_list(r#"["BufReadPre", "User LazyVimStarted"]"#).unwrap();
        assert_eq!(items[1], "User LazyVimStarted");
    }

    #[test]
    fn test_parse_cli_string_list_malformed_json_errors() {
        // "[" で始まっていると JSON として扱うので、壊れた JSON はエラー
        let err = parse_cli_string_list(r#"[BufReadPre, BufNewFile]"#).unwrap_err();
        assert!(err.to_string().contains("JSON"));
    }

    #[test]
    fn test_parse_cli_string_list_trims_and_ignores_empty() {
        let items = parse_cli_string_list("  a  ,  ,b,").unwrap();
        assert_eq!(items, vec!["a".to_string(), "b".to_string()]);
    }

    // -----------------------------------------------------------------
    // --on-map CLI パーサ / writer のテスト
    // -----------------------------------------------------------------

    #[test]
    fn test_parse_on_map_cli_simple_single_string() {
        let specs = parse_on_map_cli("<leader>f").unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].lhs, "<leader>f");
        assert!(specs[0].mode.is_empty());
        assert_eq!(specs[0].desc, None);
    }

    #[test]
    fn test_parse_on_map_cli_comma_separated() {
        let specs = parse_on_map_cli("<leader>f, <leader>g ,<leader>h").unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].lhs, "<leader>f");
        assert_eq!(specs[1].lhs, "<leader>g");
        assert_eq!(specs[2].lhs, "<leader>h");
    }

    #[test]
    fn test_parse_on_map_cli_json_array_of_strings() {
        let specs = parse_on_map_cli(r#"["<leader>f", "<leader>g"]"#).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].lhs, "<leader>f");
        assert_eq!(specs[1].lhs, "<leader>g");
    }

    #[test]
    fn test_parse_on_map_cli_json_single_object() {
        let specs =
            parse_on_map_cli(r#"{ "lhs": "<space>d", "mode": ["n", "x"], "desc": "Delete" }"#)
                .unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].lhs, "<space>d");
        assert_eq!(specs[0].mode, vec!["n".to_string(), "x".to_string()]);
        assert_eq!(specs[0].desc.as_deref(), Some("Delete"));
    }

    #[test]
    fn test_parse_on_map_cli_json_object_mode_as_string() {
        let specs = parse_on_map_cli(r#"{ "lhs": "<leader>v", "mode": "v" }"#).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].lhs, "<leader>v");
        assert_eq!(specs[0].mode, vec!["v".to_string()]);
    }

    #[test]
    fn test_parse_on_map_cli_json_array_mixed() {
        let specs = parse_on_map_cli(
            r#"[
                "<leader>a",
                { "lhs": "<leader>b", "mode": "x" },
                { "lhs": "<leader>c", "mode": ["n", "v"], "desc": "C" }
            ]"#,
        )
        .unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].lhs, "<leader>a");
        assert!(specs[0].mode.is_empty());
        assert_eq!(specs[1].lhs, "<leader>b");
        assert_eq!(specs[1].mode, vec!["x".to_string()]);
        assert_eq!(specs[2].lhs, "<leader>c");
        assert_eq!(specs[2].mode, vec!["n".to_string(), "v".to_string()]);
        assert_eq!(specs[2].desc.as_deref(), Some("C"));
    }

    #[test]
    fn test_parse_on_map_cli_json_object_missing_lhs_errors() {
        let err = parse_on_map_cli(r#"{ "mode": ["n"] }"#).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("lhs"));
    }

    #[test]
    fn test_set_plugin_map_field_single_simple_writes_string() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        let specs = vec![MapSpec {
            lhs: "<leader>f".to_string(),
            mode: Vec::new(),
            desc: None,
        }];
        set_plugin_map_field(&mut doc, "owner/a", specs).unwrap();
        let result = doc.to_string();
        assert!(
            result.contains("on_map = \"<leader>f\""),
            "simple single spec should write as plain string: {}",
            result
        );
    }

    #[test]
    fn test_set_plugin_map_field_with_mode_writes_inline_table() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        let specs = vec![MapSpec {
            lhs: "<space>d".to_string(),
            mode: vec!["n".to_string(), "x".to_string()],
            desc: Some("Delete".to_string()),
        }];
        set_plugin_map_field(&mut doc, "owner/a", specs).unwrap();
        let result = doc.to_string();
        assert!(
            result.contains("lhs = \"<space>d\""),
            "should include lhs field: {}",
            result
        );
        assert!(
            result.contains("mode = [\"n\", \"x\"]") || result.contains("mode = [ \"n\", \"x\" ]"),
            "should include mode array: {}",
            result
        );
        assert!(
            result.contains("desc = \"Delete\""),
            "should include desc: {}",
            result
        );
    }

    #[test]
    fn test_set_plugin_map_field_mixed_writes_array_of_mixed() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        let specs = vec![
            MapSpec {
                lhs: "<leader>a".to_string(),
                mode: Vec::new(),
                desc: None,
            },
            MapSpec {
                lhs: "<leader>b".to_string(),
                mode: vec!["n".to_string(), "x".to_string()],
                desc: Some("B".to_string()),
            },
        ];
        set_plugin_map_field(&mut doc, "owner/a", specs).unwrap();
        let result = doc.to_string();
        // 配列 literal 内に単純文字列とインラインテーブルが混在
        assert!(
            result.contains("\"<leader>a\""),
            "simple item as string: {}",
            result
        );
        assert!(
            result.contains("lhs = \"<leader>b\""),
            "full item as inline table: {}",
            result
        );
        assert!(result.contains("desc = \"B\""));
    }

    #[test]
    fn test_set_plugin_list_field_multiple_writes_as_array() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        set_plugin_list_field(
            &mut doc,
            "owner/a",
            "on_event",
            vec!["BufRead".to_string(), "BufNewFile".to_string()],
        )
        .unwrap();
        let result = doc.to_string();
        assert!(
            result.contains("on_event = ["),
            "複数要素は配列として書かれるべき: {}",
            result
        );
        assert!(result.contains("\"BufRead\""));
        assert!(result.contains("\"BufNewFile\""));
    }

    #[test]
    fn test_update_plugin_config() {
        let toml = r#"[[plugins]]
url = "test/plugin"
lazy = false"#;
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        update_plugin_config(
            &mut doc,
            "test/plugin",
            Some(true),
            Some(true),
            None,
            None,
            Some("v1.0".to_string()),
        )
        .unwrap();
        let result = doc.to_string();
        assert!(result.contains("lazy = true"));
        assert!(result.contains("merge = true"));
        assert!(result.contains("rev = \"v1.0\""));
    }

    // -----------------------------------------------------------------
    // build コマンドのテスト
    // -----------------------------------------------------------------

    #[test]
    fn test_find_unused_repos() {
        let root = tempdir().unwrap();
        // `cache_root` を root にして、標準の {cache_root}/plugins/repos/ 下に配置する
        let repos_dir = root.path().join("plugins/repos");
        std::fs::create_dir_all(&repos_dir).unwrap();
        let used_dir = repos_dir.join("github.com/used/plugin");
        let unused_dir = repos_dir.join("github.com/unused/plugin");
        std::fs::create_dir_all(used_dir.join(".git")).unwrap();
        std::fs::create_dir_all(unused_dir.join(".git")).unwrap();
        let config = Config {
            vars: None,
            options: Options::default(),
            plugins: vec![Plugin {
                url: "used/plugin".to_string(),
                ..Default::default()
            }],
        };
        let unused = find_unused_repos(&config, root.path(), &repos_dir).unwrap();
        assert_eq!(unused.len(), 1);
        assert!(unused[0].to_string_lossy().contains("unused"));
    }

    #[test]
    fn test_find_unused_repos_respects_custom_dst_inside_repos_dir() {
        // `plugin.dst` で canonical_path と違う場所に clone してる場合でも、
        // その場所は "used" として保護されること。
        let root = tempdir().unwrap();
        let repos_dir = root.path().join("plugins/repos");
        std::fs::create_dir_all(&repos_dir).unwrap();
        let custom = repos_dir.join("custom-slot/my-plugin");
        std::fs::create_dir_all(custom.join(".git")).unwrap();
        let config = Config {
            vars: None,
            options: Options::default(),
            plugins: vec![Plugin {
                url: "owner/my-plugin".to_string(),
                dst: Some(custom.to_string_lossy().to_string()),
                ..Default::default()
            }],
        };
        let unused = find_unused_repos(&config, root.path(), &repos_dir).unwrap();
        assert!(
            unused.is_empty(),
            "custom dst must be protected, got {:?}",
            unused
        );
    }

    #[test]
    fn test_find_unused_repos_preserves_nested_git_inside_used_plugin() {
        // 設定済みプラグインのクローン配下にある submodule 等の `.git` は
        // 削除候補にしないこと (repo_root が used プラグインの子孫なら保護)。
        let root = tempdir().unwrap();
        let repos_dir = root.path().join("plugins/repos");
        std::fs::create_dir_all(&repos_dir).unwrap();
        let plugin_dir = repos_dir.join("github.com/used/plugin");
        std::fs::create_dir_all(plugin_dir.join(".git")).unwrap();
        // submodule
        let submodule = plugin_dir.join("deps/sub");
        std::fs::create_dir_all(submodule.join(".git")).unwrap();
        let config = Config {
            vars: None,
            options: Options::default(),
            plugins: vec![Plugin {
                url: "used/plugin".to_string(),
                ..Default::default()
            }],
        };
        let unused = find_unused_repos(&config, root.path(), &repos_dir).unwrap();
        assert!(
            unused.is_empty(),
            "submodule .git must not be considered unused, got {:?}",
            unused
        );
    }

    #[test]
    fn test_prune_unused_repos_removes_listed_dirs() {
        let root = tempdir().unwrap();
        let a = root.path().join("a/.git");
        let b = root.path().join("b/.git");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let targets = vec![
            a.parent().unwrap().to_path_buf(),
            b.parent().unwrap().to_path_buf(),
        ];
        prune_unused_repos(&targets);
        assert!(!targets[0].exists());
        assert!(!targets[1].exists());
    }

    #[test]
    fn test_prune_unused_repos_empty_slice_noop() {
        // 空でもクラッシュしないこと
        prune_unused_repos(&[]);
    }

    #[test]
    fn test_plural_helper() {
        assert_eq!(plural("dir", "dirs", 0), "dirs");
        assert_eq!(plural("dir", "dirs", 1), "dir");
        assert_eq!(plural("dir", "dirs", 2), "dirs");
    }

    #[test]
    fn test_parse_config_auto_clean_defaults_to_false() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
"#;
        let config = crate::config::parse_config(toml).unwrap();
        assert!(!config.options.auto_clean);
    }

    #[test]
    fn test_parse_config_accepts_auto_clean_true() {
        let toml = r#"
[options]
auto_clean = true

[[plugins]]
url = "owner/repo"
"#;
        let config = crate::config::parse_config(toml).unwrap();
        assert!(config.options.auto_clean);
    }

    // ================================================================
    // update_log wiring
    // ================================================================
}
