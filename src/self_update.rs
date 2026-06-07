//! `rvpm self-update` + the background auto-update check (#125, #217).
//!
//! `run_self_update` drives the explicit command; `maybe_spawn_auto_update_check`
//! / `finalize_auto_update_check` run the throttled background version probe that
//! prints the update banner after a command completes. Extracted verbatim from
//! the monolithic lib.rs.

use anyhow::Result;

/// 自動 update の handle。 結果は `finalize_auto_update_check` で消費する。
pub(crate) enum AutoUpdateHandle {
    /// notify mode: throttle window 内で fetch skip、 過去の cache が新版を示して
    /// いるので banner だけ出す。
    CachedAvailable {
        checker: kaishin::Checker,
        latest: kaishin::LatestRelease,
    },
    /// notify mode: バックグラウンドで GitHub API を叩いてる。 join 結果に応じて
    /// 状態保存 + banner。
    Pending {
        checker: kaishin::Checker,
        handle: tokio::task::JoinHandle<Result<Option<kaishin::LatestRelease>, anyhow::Error>>,
        /// タイムアウト時のフォールバック用。
        cached_latest: Option<kaishin::LatestRelease>,
    },
    /// install mode: バックグラウンドで check + download + 差し替えを silent 実行。
    /// 完了して新版を入れたら 1 行だけ通知する (走行中プロセスは旧バイナリのまま、
    /// 反映は次回起動)。
    Installing {
        handle: tokio::task::JoinHandle<Result<Option<kaishin::LatestRelease>, anyhow::Error>>,
    },
}

/// `RVPM_NO_AUTOUPDATE` が「有効」な値で設定されているか。 `0` / `false` / 空白は
/// 無効扱い (= 自動更新を止めない)。 config の `auto_update` より優先する kill-switch。
pub(crate) fn auto_update_disabled_by_env() -> bool {
    match std::env::var("RVPM_NO_AUTOUPDATE") {
        Ok(v) => {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
        Err(_) => false,
    }
}

/// config の `auto_update` (旧 `auto_update_check`) に応じて background 処理を起動する。
///
/// - `off` (または env `RVPM_NO_AUTOUPDATE`): 何もせず `None`。
/// - `notify`: GitHub API を叩いて新版があれば banner で案内する (install しない)。
///   throttle 内でも cache が新版を示していれば banner 用 handle を返す。
/// - `install`: `Checker::auto_update` を spawn し、 silent に download + 差し替え。
///   完了したら finalize で 1 行通知する。
///
/// 失敗時 (config 読めない等) は `None` を返して silent skip (resilience)。
pub(crate) async fn maybe_spawn_auto_update_check() -> Option<AutoUpdateHandle> {
    use crate::config::AutoUpdateMode;

    // config が読めない / 失敗するケースもあるので resilience。 panic は絶対にしない。
    let config_path = config_path_for_auto_check()?;
    // async fn: use tokio::fs so the blocking read doesn't stall the executor
    // thread while the background update check is being set up (#226).
    let toml_str = tokio::fs::read_to_string(&config_path).await.ok()?;
    let cfg = crate::config::parse_config(&toml_str).ok()?;

    // env kill-switch は config より優先。
    let mode = if auto_update_disabled_by_env() {
        AutoUpdateMode::Off
    } else {
        cfg.options.update_mode()
    };
    if mode == AutoUpdateMode::Off {
        return None;
    }

    let interval = cfg
        .options
        .update_check_interval
        .as_deref()
        .and_then(|s| kaishin::parse_interval(s).ok())
        .unwrap_or_else(kaishin::default_interval);

    let cache_root = crate::paths::resolve_cache_root(cfg.options.cache_root.as_deref());
    let opts = kaishin::KaishinOptions::new(
        "yukimemi",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    );
    let checker = kaishin::Checker::new(env!("CARGO_PKG_NAME"), opts)
        .interval(interval)
        .state_path(cache_root.join("last_update_check.json"));

    match mode {
        // 上で early-return 済みだが、 網羅性のため。
        AutoUpdateMode::Off => None,
        AutoUpdateMode::Notify => {
            if !checker.should_check() {
                // throttle 内 — cache から判定
                if let Some(latest) = checker.cached_update() {
                    return Some(AutoUpdateHandle::CachedAvailable { checker, latest });
                }
                return None;
            }
            // fetch を spawn
            let cached_latest = checker.cached_update();
            let checker_clone = checker.clone();
            let handle = tokio::spawn(async move { checker_clone.check_and_save().await });
            Some(AutoUpdateHandle::Pending {
                checker,
                handle,
                cached_latest,
            })
        }
        AutoUpdateMode::Install => {
            // `auto_update` は self-throttle + cross-process lock + silent install
            // まで面倒を見る。 due でなければ即 Ok(None)。 dev build は no-op。
            let handle = tokio::spawn(async move { checker.auto_update().await });
            Some(AutoUpdateHandle::Installing { handle })
        }
    }
}

/// `rvpm` の config file の path を解決する (auto-check 用)。
/// 既存の `crate::paths::rvpm_config_path()` ヘルパに委譲して、 `.config/rvpm/...` のハードコードを
/// 避ける (CodeRabbit PR #126 指摘の coding guideline 整合)。
pub(crate) fn config_path_for_auto_check() -> Option<std::path::PathBuf> {
    let p = crate::paths::rvpm_config_path();
    p.exists().then_some(p)
}

/// background fetch を join + banner 出力 (#125)。
/// timeout は 1 秒 — fetch が遅ければ次回に回す。
pub(crate) async fn finalize_auto_update_check(handle: AutoUpdateHandle) {
    match handle {
        AutoUpdateHandle::CachedAvailable { checker, latest } => {
            eprintln!("\n{}", checker.format_banner(&latest));
        }
        AutoUpdateHandle::Pending {
            checker,
            handle,
            cached_latest,
        } => {
            // 1 秒 timeout で結果を待つ。 タイムアウトは silent skip。
            // kaishin 0.4 で check_and_save が Option を返すようになったので、
            // ここでの is_update_available 重複チェックは不要 (Ok(None) = 更新無し)。
            let res = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
            match res {
                Ok(Ok(Ok(Some(latest)))) => {
                    eprintln!("\n{}", checker.format_banner(&latest));
                }
                Ok(Ok(Ok(None))) => {
                    // fetch 成功 + 更新無し: cache へのフォールバックは不要
                    // (最新が現在版に追いついた直後など、 cache は古いだけ)。
                }
                _ => {
                    // タイムアウトや fetch エラー時のみ cache を試す。
                    if let Some(latest) = cached_latest {
                        eprintln!("\n{}", checker.format_banner(&latest));
                    }
                }
            }
        }
        AutoUpdateHandle::Installing { handle } => {
            // 実際に download が走るのは新版がある時 (= throttle ごとに高々 1 回)
            // だけで、 大半の起動では `auto_update` が即 Ok(None) を返すのでこの待ちは
            // 一瞬で抜ける。 download 中の稀な起動でも `rvpm list` 等を長時間ブロック
            // しないよう、 待ちは短い上限 (5 秒) に留める。 download は起動時から
            // コマンド本体と並行で走っているので、 大抵はこの時点で完了済み。
            // タイムアウト時は黙って次回に回す — 中断しても self_replace は atomic
            // なのでバイナリは壊れない (遅い回線では次の機会か手動 self-update に委ねる)。
            let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
            if let Ok(Ok(Ok(Some(latest)))) = res {
                let v = latest.tag_name.trim_start_matches('v');
                eprintln!("\n\u{2713} rvpm {v} installed in the background — restart to apply.");
            }
        }
    }
}

/// `rvpm self-update` (#125)。
pub(crate) async fn run_self_update(yes: bool, check_only: bool) -> Result<()> {
    let opts = kaishin::KaishinOptions::new(
        "yukimemi",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    );
    let upd_opts = kaishin::UpdateOptions::new()
        .yes(yes)
        .check_only(check_only);
    // non_interactive フラグは rvpm ではまだ CmdCtx に持っていないため、デフォルト false。
    // (必要になれば CmdCtx に追加して伝搬させる)
    kaishin::run_self_update(&opts, upd_opts).await
}
