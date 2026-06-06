//! Merge-mode decision + view/merged tree construction (#119, #217).
//!
//! `decide_merge_mode` classifies each plugin into a [`PluginMergeMode`], and
//! `dispatch_plugin_merge` / `build_view_atomically` build the corresponding
//! hard-linked rtp tree under `<cache_root>/plugins/{merged,views}` atomically.
//! `record_merge_result` / `print_merge_conflicts` aggregate first-wins merge
//! conflicts for `rvpm doctor`. Extracted verbatim from the monolithic lib.rs.

use anyhow::Context;
use std::path::{Path, PathBuf};

/// プラグインの merge モードを決定する純粋関数 (#119)。
///
/// 戻り値の 4 状態がそのまま「sync 時に何を作って rtp に何を載せるか」と
/// 1:1 対応する:
///
/// | 戻り値                  | sync で配置する物                         | rtp に乗せる path                     |
/// |-------------------------|-------------------------------------------|---------------------------------------|
/// | `Full`                  | `merged/` 配下に全 rtp dir を集約         | `merged/` (Phase 5 で 1 度)            |
/// | `ViewWithDoc`           | `views/<plug>/` (`doc/` 含む全 rtp dir)   | `views/<plug>/` (eager: 起動時 / lazy: trigger) |
/// | `ViewWithoutDoc`        | `views/<plug>/` (`doc/` 除く) + `merged/doc/` への doc hard-link | 同上 (但し doc は merged/ 経由で常時) |
/// | `None`                  | 何もしない                                | (リソースに該当しない)                |
///
/// 解決ルール:
/// - `merge=true && eager` は無条件で `Full` (per-plugin/ global の `merge_doc` 設定は無視)
/// - それ以外は `effective_merge_doc = pp_merge_doc.unwrap_or(global_merge_doc)`:
///   - true なら `ViewWithoutDoc`
///   - false なら `ViewWithDoc`
/// - ただし `merge=false` で `effective_merge_doc=false` の組合せは `ViewWithDoc`
///   (= clone と等価な doc 入り view) になる。挙動は今までの「clone path を rtp に append」
///   と等価だが、 rtp は常に `views/` 経由で入るので mental model がシンプル化される。
///
/// `cond` プリパスは呼び出し側 (`disable_merge_if_cond`) で適用済みである前提。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PluginMergeMode {
    /// eager + merge=true: `merged/` に全部集約。
    Full,
    /// `views/<plug>/` を doc/ 込みで build、 rtp に view を載せる。
    /// 旧来の clone path 直 rtp:append と等価な挙動だが view 経由に統一。
    ViewWithDoc,
    /// `views/<plug>/` を doc/ 抜きで build。 doc/ ファイルは別途
    /// `merged/doc/` に hard-link で集約する (= 主目的)。
    ViewWithoutDoc,
}

pub(crate) fn decide_merge_mode(
    plugin_merge: bool,
    plugin_lazy: bool,
    plugin_merge_doc: Option<bool>,
    merge_doc_default: bool,
) -> PluginMergeMode {
    if plugin_merge && !plugin_lazy {
        return PluginMergeMode::Full;
    }
    let effective = plugin_merge_doc.unwrap_or(merge_doc_default);
    if effective {
        PluginMergeMode::ViewWithoutDoc
    } else {
        PluginMergeMode::ViewWithDoc
    }
}

/// `crate::link` の各 merge ヘルパを呼び、衝突を勝者 (先に同じ path を置いた plugin)
/// とセットで `conflicts` に積む共通処理。 merge 自体が失敗した場合は stderr に
/// warn を流すがエラーにはしない (resilience)。
///
/// `ownership` は merged/ (もしくは views/<plug>/) 上の relative path → 勝者
/// plugin 名の shared map。 ただし views は per-plugin に独立なので衝突は通常
/// 発生しない (全 view に固有 dst を渡す)。 merged/ 用は呼び出し側で同じ map を
/// 使い回して、 後続 plugin の衝突時に勝者を lookup する。
pub(crate) fn record_merge_result(
    plugin_name: &str,
    result: anyhow::Result<crate::link::MergeResult>,
    ownership: &mut std::collections::HashMap<PathBuf, String>,
    conflicts: &mut Vec<crate::merge_conflicts::MergeConflictReport>,
) {
    match result {
        Ok(r) => {
            for placed in r.placed {
                ownership.insert(placed, plugin_name.to_string());
            }
            for c in r.conflicts {
                let winner = ownership.get(&c.relative).cloned();
                // 自己 conflict (winner == loser) は記録しない:
                // ViewWithoutDoc で既に merged/doc/ に doc を配置した plugin が
                // promote_lazy_to_eager で eager に昇格して Full merge を再実行する
                // ようなケース。 同じ plugin が再度同じファイルを置こうとして
                // first-wins skip するだけなので、 ユーザーには false-positive。
                if winner.as_deref() == Some(plugin_name) {
                    continue;
                }
                let rel = c.relative.to_string_lossy().replace('\\', "/");
                conflicts.push(crate::merge_conflicts::MergeConflictReport {
                    loser: plugin_name.to_string(),
                    winner,
                    relative: rel,
                });
            }
        }
        Err(e) => {
            eprintln!("\u{26a0} merge failed for {}: {}", plugin_name, e);
        }
    }
}

/// suffix-付き sibling path を生成する。 `view_dir.with_extension(suffix)` だと
/// `plugin.nvim` と `plugin.vim` が両方 `plugin.<suffix>` に化けて衝突するので
/// (Gemini PR #129 指摘)、 末尾に `.<suffix>` を appending する形で安全に作る。
pub(crate) fn sibling_with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

/// View dir を atomic に置き換えるためのヘルパ。
///
/// 旧実装は `remove_dir_all(view_dir)` → `merge_plugin_view*` で再 link、 という
/// 2 段階でやってたが、 これだと **削除完了から再 link 完了の間 view dir が空**
/// になる窓ができる。 Neovide が走ってる状態で `rvpm generate` (例: `:Rvpm` TUI
/// から `c` キーで config 編集後) が動くと、 その瞬間に lazy plugin の load_lazy
/// が `dofile(after)` で view 配下の lua module を require した場合、 `No such
/// file or directory` で失敗する race が観測された (#128)。
///
/// 修正アプローチ (POSIX / Windows で 2 経路):
/// - **POSIX**: `rename(tmp, view)` は existing dir を atomic 置換できる。
///   tmp に build → 直接 rename で 1-step 置換。 view_dir が空になる瞬間ゼロ。
/// - **Windows**: `rename` で existing dir を replace できないので 2-step:
///   既存 view を `.rvpm-old` に退避 → tmp を view に rename → `.rvpm-old` 削除。
///   退避と rename の間に小さい window はあるが、 旧実装の delete→build より
///   遥かに短い (rename はメタデータ更新のみ)。
pub(crate) fn atomic_replace_view_dir<F>(view_dir: &Path, build: F) -> anyhow::Result<()>
where
    F: FnOnce(&Path) -> anyhow::Result<()>,
{
    let tmp_dir = sibling_with_suffix(view_dir, "rvpm-tmp");
    let old_dir = sibling_with_suffix(view_dir, "rvpm-old");
    // 前回の sync が中途で死んでて残骸が残ってるケースに備えて事前 cleanup。
    //
    // ここを silent fail (`let _ = remove_dir_all`) にすると致命的な汚染が起きる:
    // Windows で Neovim が走ってて hard-link を掴んでいる等で remove が失敗すると、
    // 前回 run の `*.rvpm-tmp` 残骸が残ったまま build が走り、merge は「この run で
    // 誰も置いていないファイル」の上に first-wins で重なる。結果として merge 衝突
    // レポートが winner=<unknown> で大量に出る (実際にユーザー環境で観測)。
    // remove を verify して、消し切れなければ build に進まず Err を返す
    // — 呼び出し側 (run_generate / build_view_atomically) は既存 view/merged を
    // 温存したままこの run の更新を諦め、次 run (ロック解放後) で自己修復する。
    ensure_absent(&tmp_dir)?;
    ensure_absent(&old_dir)?;
    // Step 1: tmp に新規 build。
    build(&tmp_dir)?;
    // Step 2: 既存 view を .old に退避 → tmp を view に rename → .old 削除。
    //
    // 旧実装は POSIX なら `rename(tmp, view)` で直接置換できると想定していたが、
    // Linux の rename(2) は dst が非空ディレクトリの場合 ENOTEMPTY を返すため
    // 2 回目以降の sync で必ず失敗していた (#158)。
    // Windows 同様に「退避 → rename → 削除」の 3-step に統一する。
    // view_dir が空になる瞬間は退避 rename と tmp rename の間に微小窓として残るが、
    // いずれも rename (メタデータ更新のみ) なので旧実装の delete→build より十分短い。
    if view_dir.exists() {
        std::fs::rename(view_dir, &old_dir)?;
    }
    if let Err(e) = std::fs::rename(&tmp_dir, view_dir) {
        // Step 3 失敗時は .old を view に戻して整合保つ (best-effort)。
        let _ = std::fs::rename(&old_dir, view_dir);
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(e.into());
    }
    let _ = std::fs::remove_dir_all(&old_dir);
    Ok(())
}

/// `path` を再帰削除し、削除後も残っていれば Err を返す。
///
/// `remove_dir_all` は Windows でファイルがロックされている等のとき、配下を
/// 一部だけ消した状態で中断して Err を返すことがある (atomic ではない)。さらに
/// 「既に存在しない」場合は Ok を返すので、戻り値だけでは「消し切れたか」を判定
/// できない。そこで remove のあと存在を再確認し、残っていれば残骸混入を防ぐため
/// 明示的に fail させる。呼び出し側はこの Err を「この run の更新を諦めて既存状態を
/// 温存する」シグナルとして扱う (次 run で自己修復)。
pub(crate) fn ensure_absent(path: &Path) -> anyhow::Result<()> {
    // `Path::exists()` は **broken symlink で false を返す** (target の metadata を
    // 引くため)。残骸が dangling symlink だと exists() が false → 削除も検証もすり抜け、
    // 後段の rename/create が "already exists" で落ちる。link.rs でも既出の罠。
    // `symlink_metadata()` は symlink 自体を stat するので、壊れた symlink も含めて
    // 「path に何かあるか」を正しく判定できる。
    if path.symlink_metadata().is_ok() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("remove stale dir {}", path.display()))?;
    }
    if path.symlink_metadata().is_ok() {
        anyhow::bail!(
            "{} still exists after remove_dir_all (locked by another process?)",
            path.display()
        );
    }
    Ok(())
}

/// `PluginMergeMode` に従って sync 時のリンク tree を作る (#119 統一案)。
///
/// - `Full` → `merged/` に全 rtp dir を集約
/// - `ViewWithDoc` → `views/<plug>/` に全 rtp dir (doc/ 込み) を集約。 merged/ には何も置かない。
/// - `ViewWithoutDoc` → `views/<plug>/` に全 rtp dir (doc/ 抜き) を集約 +
///   `merged/doc/` に doc/ を集約。
///
/// 既存ファイルは別 view への上書きの心配が無い (view dir は per-plugin に新規) ので
/// view 側の衝突は通常発生しない。 merged/ 側だけが ownership 共有の対象。
pub(crate) fn dispatch_plugin_merge(
    mode: PluginMergeMode,
    src: &Path,
    merged_dir: &Path,
    view_dir: &Path,
    plugin_name: &str,
    ownership: &mut std::collections::HashMap<PathBuf, String>,
    conflicts: &mut Vec<crate::merge_conflicts::MergeConflictReport>,
) {
    // View 系の rebuild では既存 view_dir を削除してから link し直す:
    // `merge_plugin*` は first-wins + hard-link only なので、 残骸が残ったままだと
    // (a) upstream で削除されたファイルが view にゴミとして残る、
    // (b) 旧 mode で置かれた `doc/` が ViewWithoutDoc に切り替わっても残ってしまい
    //     rtp に再度 doc が漏れる、 という汚染が起きる。 CodeRabbit PR #120 指摘。
    // Full は merged/ に向くので個別 view 削除は不要 (run_generate 側で全消し済か、
    // それ以外の plugin との衝突報告に乗る)。
    match mode {
        PluginMergeMode::Full => {
            let r = crate::link::merge_plugin(src, merged_dir);
            record_merge_result(plugin_name, r, ownership, conflicts);
        }
        PluginMergeMode::ViewWithDoc => {
            // view 側は per-plugin 専用 dir なので、 別 plugin との衝突は発生しない。
            // atomic_replace_view_dir で tmp に build → atomic rename することで、
            // Neovim が走ってる状態で `rvpm generate` が動いても、 view dir が
            // 空になる窓を作らない (lazy plugin の require / autoload race を回避)。
            build_view_atomically(
                src,
                view_dir,
                plugin_name,
                conflicts,
                crate::link::merge_plugin_view,
            );
        }
        PluginMergeMode::ViewWithoutDoc => {
            build_view_atomically(
                src,
                view_dir,
                plugin_name,
                conflicts,
                crate::link::merge_plugin_view_no_doc,
            );
            // 2) doc/ だけ merged/ に集約 (これが merge_doc=true の本命)
            let r = crate::link::merge_plugin_doc_only(src, merged_dir);
            record_merge_result(plugin_name, r, ownership, conflicts);
        }
    }
}

/// `views/<plug>/` を tmp 経由で atomic rebuild するヘルパー。
///
/// `merge_view_fn` で plugin の rtp dir を tmp に集約し、 続けて `link_dotgit_into_view`
/// で `.git` を repos clone から junction / symlink で露出する (blink.cmp 等が
/// 自分の tag 状態を `vim.fs.root('.git')` で判定するため。 failure は warn のみ
/// で続行 = sync 全体を止めない resilience)。
///
/// 元々 `dispatch_plugin_merge` の `ViewWithDoc` / `ViewWithoutDoc` で同じ block が
/// 2 回登場していたので 1 関数に括った (Gemini PR #135 medium)。
pub(crate) fn build_view_atomically(
    src: &Path,
    view_dir: &Path,
    plugin_name: &str,
    conflicts: &mut Vec<crate::merge_conflicts::MergeConflictReport>,
    merge_view_fn: fn(&Path, &Path) -> anyhow::Result<crate::link::MergeResult>,
) {
    let mut view_ownership: std::collections::HashMap<PathBuf, String> =
        std::collections::HashMap::new();
    let mut view_conflicts: Vec<crate::merge_conflicts::MergeConflictReport> = Vec::new();
    let mut merge_result: Option<anyhow::Result<crate::link::MergeResult>> = None;
    let atomic_res = atomic_replace_view_dir(view_dir, |tmp| {
        let m = merge_view_fn(src, tmp)?;
        if let Err(e) = crate::link::link_dotgit_into_view(src, tmp) {
            eprintln!(
                "\u{26a0} failed to expose .git in view {}: {}",
                view_dir.display(),
                e
            );
        }
        merge_result = Some(Ok(m));
        Ok(())
    });
    if let Err(e) = atomic_res {
        merge_result = Some(Err(e.context("atomic view replace failed")));
    }
    if let Some(r) = merge_result {
        record_merge_result(plugin_name, r, &mut view_ownership, &mut view_conflicts);
    }
    conflicts.extend(view_conflicts);
}

/// 収集した衝突を plugin ごとにグループ化して stderr にサマリ出力する。
/// 各ファイル行には `(kept: <winner>)` を付けて、どちらが first-wins で
/// 残ったかをユーザーが即座に分かるようにする。
pub(crate) fn print_merge_conflicts(conflicts: &[crate::merge_conflicts::MergeConflictReport]) {
    if conflicts.is_empty() {
        return;
    }
    let mut by_plugin: std::collections::BTreeMap<
        &str,
        Vec<&crate::merge_conflicts::MergeConflictReport>,
    > = std::collections::BTreeMap::new();
    for r in conflicts {
        by_plugin.entry(r.loser.as_str()).or_default().push(r);
    }
    eprintln!();
    eprintln!(
        "\u{26a0} {} merge conflict(s) across {} plugin(s) — first-wins, later entries skipped:",
        conflicts.len(),
        by_plugin.len(),
    );
    for (plugin, reports) in &by_plugin {
        eprintln!(
            "  {} ({} file{}):",
            plugin,
            reports.len(),
            crate::plural_s(reports.len())
        );
        for r in reports {
            let winner = r.winner.as_deref().unwrap_or("<unknown>");
            eprintln!("    {}  (kept: {})", r.relative, winner);
        }
    }
    // winner=<unknown> は「この run で誰も置いていないファイルと衝突した」状態で、
    // 通常は前回 run の merged 残骸が混入したときに出る。`ensure_absent` でこの
    // 混入は構造的に防いでいるが、何らかの理由で残骸が残った場合に再 sync で
    // 直ることをユーザーに案内する (本物の cross-plugin 衝突なら winner は実名が出る)。
    if conflicts.iter().any(|r| r.winner.is_none()) {
        eprintln!(
            "  note: \"<unknown>\" winners usually mean stale merged residue — \
             re-run `rvpm sync` to rebuild from scratch; if it persists, please report it."
        );
    }
}
