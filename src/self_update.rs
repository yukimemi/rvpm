//! Self-update support for rvpm (#125).
//!
//! 提供する機能は 2 つ:
//!
//! 1. `rvpm self-update` — GitHub releases の latest と現バイナリ版を比較して、
//!    新版があれば prompt → install。 install 方法は `current_exe()` のパスから
//!    検出 (cargo install / dev build / direct binary)。
//! 2. **daily auto-check banner** — 任意の `rvpm <subcommand>` 実行時に
//!    バックグラウンドで latest を fetch し、 末尾に新版案内を表示。 throttle 用の
//!    timestamp は `<cache_root>/last_update_check.json` に保存する。
//!
//! 設計方針:
//! - **Resilience**: ネットワーク失敗 / GitHub API rate limit は silent skip。
//!   通常の `rvpm sync` 等を絶対に止めない。
//! - **CLI 文化に沿う**: 自動 install はしない。 banner で通知 → ユーザーが
//!   `rvpm self-update` を能動的に叩く。
//! - **TLS pin 整合**: `self_update` crate を `default-features = false` +
//!   `rustls` で組み込み、 rvpm 全体の `rustls-tls` 統一に乗せる。

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// GitHub releases API の `releases/latest` から取得する最小限のフィールド。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LatestRelease {
    /// `v3.31.4` のような tag 文字列 (先頭 `v` 込み)。
    pub tag_name: String,
    /// release page の human-readable URL (banner に表示する)。
    #[serde(default)]
    pub html_url: String,
}

/// バイナリのインストール方法。 `current_exe()` のパス文字列から検出する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    /// `cargo install rvpm` 経由 (`~/.cargo/bin/rvpm`)。
    /// `cargo install rvpm --force` で再 install する。
    CargoInstall,
    /// `target/debug/...` / `target/release/...` 配下の開発ビルド。
    /// 自動更新は拒否し、 manual rebuild を案内する。
    DevBuild,
    /// それ以外 (GitHub release から落とした binary を任意の場所に置いた等)。
    /// `self_update` crate で atomic 自己置換する。
    DirectBinary,
}

/// `<cache_root>/last_update_check.json` に永続化される throttle 用の状態。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateCheckState {
    /// 最後に GitHub API を叩いた Unix timestamp (秒)。
    pub last_checked_unix: u64,
    /// 直近の API 応答で得た latest tag (例: `v3.31.4`)。
    /// 次の check が throttle で skip されても、 banner は引き続き出せるようにキャッシュする。
    pub last_known_latest: Option<String>,
}

/// 自動 check の default 間隔 (24h)。
pub fn default_interval() -> Duration {
    Duration::from_secs(86400)
}

/// `[options] update_check_interval` を Duration に変換する。
/// `humantime` のフォーマット (`"24h"`, `"30m"`, `"1d"` 等) を受け付ける。
pub fn parse_interval(s: &str) -> Result<Duration> {
    Ok(humantime::parse_duration(s)?)
}

/// `current_exe()` のパスから install 方法を推定する。
///
/// 判定順:
/// 1. `target/debug/` または `target/release/` を含む → `DevBuild`
///    (rvpm の開発中 build、 自己更新を拒否すべき)
/// 2. `$CARGO_HOME/bin/` (env で resolve、 未設定なら `~/.cargo/bin/`) 配下 → `CargoInstall`
///    `CARGO_HOME=/opt/rust` 等の custom 配置にも追従する (CodeRabbit PR #126 指摘)。
/// 3. fallback で `.cargo/bin/` / `cargo/bin/` 文字列を含むパス → `CargoInstall`
/// 4. それ以外 → `DirectBinary`
pub fn detect_install_method(exe: &Path) -> InstallMethod {
    let s = exe.to_string_lossy().replace('\\', "/").to_lowercase();
    if s.contains("/target/debug/") || s.contains("/target/release/") {
        return InstallMethod::DevBuild;
    }
    // CARGO_HOME を尊重する (custom 配置の cargo install を `CargoInstall` 扱いに)。
    let cargo_bin = std::env::var("CARGO_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))
        .map(|p| {
            p.join("bin")
                .to_string_lossy()
                .replace('\\', "/")
                .to_lowercase()
        });
    if let Some(bin) = cargo_bin
        && s.starts_with(&format!("{}/", bin))
    {
        return InstallMethod::CargoInstall;
    }
    // fallback: 上の resolve で取れなかった環境用 (HOME 不在のテスト等)。
    if s.contains("/.cargo/bin/") || s.contains("/cargo/bin/") {
        return InstallMethod::CargoInstall;
    }
    InstallMethod::DirectBinary
}

/// `current` (現バイナリの version 文字列) と `latest_tag` (`v` prefix 込みの
/// release tag) を比較し、 latest > current なら true。
///
/// semver 解析失敗時は Err を返す (caller 側は silent skip する想定)。
pub fn is_update_available(current: &str, latest_tag: &str) -> Result<bool> {
    let cur = semver::Version::parse(current)
        .map_err(|e| anyhow!("invalid current version `{}`: {}", current, e))?;
    let lat_str = latest_tag.trim_start_matches('v');
    let lat = semver::Version::parse(lat_str)
        .map_err(|e| anyhow!("invalid latest tag `{}`: {}", latest_tag, e))?;
    Ok(lat > cur)
}

/// GitHub releases API を叩いて latest release を取得する。
///
/// timeout 5 秒 / `User-Agent: rvpm/<version>`。 失敗時は caller 側で silent skip
/// 推奨 (resilience)。
pub async fn check_latest_release() -> Result<LatestRelease> {
    let url = "https://api.github.com/repos/yukimemi/rvpm/releases/latest";
    let client = reqwest::Client::builder()
        .user_agent(format!("rvpm/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(5))
        .build()?;
    let res = client.get(url).send().await?;
    if !res.status().is_success() {
        return Err(anyhow!("GitHub releases API returned {}", res.status()));
    }
    let release: LatestRelease = res.json().await?;
    Ok(release)
}

fn state_path(cache_root: &Path) -> PathBuf {
    cache_root.join("last_update_check.json")
}

/// throttle 状態を読む。 ファイル不在 / 破損時は `None` を返す
/// (= 初回扱いで check が走る)。
pub fn load_check_state(cache_root: &Path) -> Option<UpdateCheckState> {
    let content = std::fs::read_to_string(state_path(cache_root)).ok()?;
    serde_json::from_str(&content).ok()
}

/// throttle 状態を書く。
pub fn save_check_state(cache_root: &Path, state: &UpdateCheckState) -> Result<()> {
    std::fs::create_dir_all(cache_root)?;
    let json = serde_json::to_string(state)?;
    std::fs::write(state_path(cache_root), json)?;
    Ok(())
}

/// 自動 check を走らせるか。 state 不在 → 走らせる、 elapsed >= interval → 走らせる、
/// それ以外 → throttle で skip。
pub fn should_auto_check(
    state: Option<&UpdateCheckState>,
    interval: Duration,
    now: SystemTime,
) -> bool {
    let Some(state) = state else {
        return true;
    };
    let Ok(now_unix) = now.duration_since(SystemTime::UNIX_EPOCH) else {
        // 時計が逆行してる (cmos battery / VM クロック skew) — 安全側に倒して check 走らす。
        return true;
    };
    let elapsed = now_unix.as_secs().saturating_sub(state.last_checked_unix);
    elapsed >= interval.as_secs()
}

/// banner 文字列を生成する。 `LatestRelease.html_url` が空なら省略。
/// stderr 出力する想定。
pub fn format_update_banner(current: &str, latest: &LatestRelease) -> String {
    let tag = latest.tag_name.trim_start_matches('v');
    let mut s = format!(
        "\u{2699} rvpm {} available (current {}) — run `rvpm self-update` to upgrade",
        tag, current
    );
    if !latest.html_url.is_empty() {
        s.push_str(&format!("\n  release notes: {}", latest.html_url));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_detect_install_method_cargo_unix() {
        let p = PathBuf::from("/home/u/.cargo/bin/rvpm");
        assert_eq!(detect_install_method(&p), InstallMethod::CargoInstall);
    }

    #[test]
    fn test_detect_install_method_cargo_windows() {
        let p = PathBuf::from(r"C:\Users\yukimemi\.cargo\bin\rvpm.exe");
        assert_eq!(detect_install_method(&p), InstallMethod::CargoInstall);
    }

    #[test]
    fn test_detect_install_method_dev_release_build() {
        let p = PathBuf::from(
            r"C:\Users\yukimemi\src\github.com\yukimemi\rvpm\target\release\rvpm.exe",
        );
        assert_eq!(detect_install_method(&p), InstallMethod::DevBuild);
    }

    #[test]
    fn test_detect_install_method_dev_debug_build() {
        let p = PathBuf::from("/home/u/src/rvpm/target/debug/rvpm");
        assert_eq!(detect_install_method(&p), InstallMethod::DevBuild);
    }

    #[test]
    fn test_detect_install_method_direct_binary() {
        let p = PathBuf::from("/usr/local/bin/rvpm");
        assert_eq!(detect_install_method(&p), InstallMethod::DirectBinary);
    }

    #[test]
    fn test_detect_install_method_direct_binary_windows() {
        let p = PathBuf::from(r"C:\tools\rvpm.exe");
        assert_eq!(detect_install_method(&p), InstallMethod::DirectBinary);
    }

    #[test]
    fn test_detect_install_method_respects_cargo_home() {
        // `CARGO_HOME=/opt/rust` 等の custom 配置でも CargoInstall として扱う
        // (CodeRabbit PR #126 指摘)。
        // SAFETY: cargo test は thread 並列で env var を触るので、 一意の値で操作 +
        // 終了時に元に戻す。 detect_install_method は同期関数なので OK。
        let prev = std::env::var("CARGO_HOME").ok();
        // SAFETY: tests run in the same process; this `set_var` is a known
        // hazard for parallel-running env-sensitive tests. Acceptable here
        // because we restore in the same function.
        unsafe {
            std::env::set_var("CARGO_HOME", "/opt/rust");
        }
        let result = detect_install_method(&PathBuf::from("/opt/rust/bin/rvpm"));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("CARGO_HOME", v),
                None => std::env::remove_var("CARGO_HOME"),
            }
        }
        assert_eq!(result, InstallMethod::CargoInstall);
    }

    #[test]
    fn test_is_update_available_newer() {
        assert!(is_update_available("3.31.3", "v3.31.4").unwrap());
    }

    #[test]
    fn test_is_update_available_same() {
        assert!(!is_update_available("3.31.3", "v3.31.3").unwrap());
    }

    #[test]
    fn test_is_update_available_older() {
        // 何らかの理由でユーザーの version が release より新しい場合は banner 出さない。
        assert!(!is_update_available("3.32.0", "v3.31.3").unwrap());
    }

    #[test]
    fn test_is_update_available_handles_no_v_prefix() {
        // tag に `v` が無いケースも扱える。
        assert!(is_update_available("3.31.3", "3.31.4").unwrap());
    }

    #[test]
    fn test_is_update_available_minor_bump() {
        assert!(is_update_available("3.31.99", "v3.32.0").unwrap());
    }

    #[test]
    fn test_is_update_available_major_bump() {
        assert!(is_update_available("3.31.3", "v4.0.0").unwrap());
    }

    #[test]
    fn test_is_update_available_invalid_current() {
        assert!(is_update_available("not-a-version", "v3.31.4").is_err());
    }

    #[test]
    fn test_is_update_available_invalid_latest() {
        assert!(is_update_available("3.31.3", "vBROKEN").is_err());
    }

    #[test]
    fn test_parse_interval_24h() {
        assert_eq!(parse_interval("24h").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn test_parse_interval_30min() {
        assert_eq!(parse_interval("30m").unwrap(), Duration::from_secs(1800));
    }

    #[test]
    fn test_parse_interval_1d() {
        assert_eq!(parse_interval("1d").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn test_parse_interval_invalid() {
        assert!(parse_interval("not-a-duration").is_err());
    }

    #[test]
    fn test_default_interval_is_24h() {
        assert_eq!(default_interval(), Duration::from_secs(86400));
    }

    #[test]
    fn test_should_auto_check_no_state() {
        assert!(should_auto_check(
            None,
            Duration::from_secs(86400),
            SystemTime::now()
        ));
    }

    #[test]
    fn test_should_auto_check_recent_state_skipped() {
        let now = SystemTime::now();
        let now_unix = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let state = UpdateCheckState {
            last_checked_unix: now_unix - 100, // 100 秒前
            last_known_latest: None,
        };
        // interval 24h、 100 秒しか経ってないので skip
        assert!(!should_auto_check(
            Some(&state),
            Duration::from_secs(86400),
            now
        ));
    }

    #[test]
    fn test_should_auto_check_old_state_runs() {
        let now = SystemTime::now();
        let now_unix = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let state = UpdateCheckState {
            last_checked_unix: now_unix - 2 * 86400, // 2 日前
            last_known_latest: None,
        };
        assert!(should_auto_check(
            Some(&state),
            Duration::from_secs(86400),
            now
        ));
    }

    #[test]
    fn test_should_auto_check_at_exact_boundary_runs() {
        // elapsed == interval ジャストの境界も check 走らす (>= 比較)。
        let now = SystemTime::now();
        let now_unix = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let state = UpdateCheckState {
            last_checked_unix: now_unix - 86400,
            last_known_latest: None,
        };
        assert!(should_auto_check(
            Some(&state),
            Duration::from_secs(86400),
            now
        ));
    }

    #[test]
    fn test_save_and_load_check_state_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path();
        let state = UpdateCheckState {
            last_checked_unix: 1714752000,
            last_known_latest: Some("v3.31.4".to_string()),
        };
        save_check_state(cache, &state).unwrap();
        let loaded = load_check_state(cache).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn test_load_check_state_missing_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        assert!(load_check_state(tmp.path()).is_none());
    }

    #[test]
    fn test_load_check_state_malformed_json_returns_none() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(state_path(tmp.path()), "not-json").unwrap();
        assert!(load_check_state(tmp.path()).is_none());
    }

    #[test]
    fn test_save_check_state_creates_cache_dir() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("nested").join("dir");
        let state = UpdateCheckState {
            last_checked_unix: 1,
            last_known_latest: None,
        };
        save_check_state(&cache, &state).unwrap();
        assert!(cache.join("last_update_check.json").exists());
    }

    #[test]
    fn test_format_update_banner_includes_versions() {
        let release = LatestRelease {
            tag_name: "v3.31.4".to_string(),
            html_url: "https://github.com/yukimemi/rvpm/releases/tag/v3.31.4".to_string(),
        };
        let s = format_update_banner("3.31.3", &release);
        assert!(s.contains("3.31.4"));
        assert!(s.contains("3.31.3"));
        assert!(s.contains("rvpm self-update"));
        assert!(s.contains("github.com/yukimemi/rvpm/releases"));
    }

    #[test]
    fn test_format_update_banner_omits_url_when_empty() {
        let release = LatestRelease {
            tag_name: "v3.31.4".to_string(),
            html_url: String::new(),
        };
        let s = format_update_banner("3.31.3", &release);
        assert!(s.contains("3.31.4"));
        assert!(!s.contains("release notes"));
    }

    #[test]
    fn test_format_update_banner_strips_v_prefix() {
        // tag は `v3.31.4` の形でも banner では `3.31.4` で表示する (current 表記と揃える)。
        let release = LatestRelease {
            tag_name: "v3.31.4".to_string(),
            html_url: String::new(),
        };
        let s = format_update_banner("3.31.3", &release);
        assert!(s.contains("rvpm 3.31.4"), "got: {}", s);
        assert!(!s.contains("rvpm v3.31.4"), "got: {}", s);
    }
}
