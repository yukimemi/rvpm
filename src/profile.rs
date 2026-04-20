// `rvpm profile` — Neovim の `--startuptime` 出力を解析して、プラグイン単位の
// 起動時間内訳を可視化する。
//
// 処理の流れ:
// 1. `nvim --headless --startuptime <tmp> +qa` を N 回起動 (デフォルト 3)
// 2. 各 run の tmp ファイルを `parse_startuptime` で `SourceEntry` 列に変換
// 3. プラグインパスの prefix match で `PluginStats` に集約
// 4. 平均を取って `ProfileReport` を作成
// 5. TUI / JSON / plain text で出力
//
// 設計上のポイント:
// - parser / aggregator は pure 関数でテスト可能。async / IO 依存を持たない。
// - plugin 帰属判定は `<repos_dir>/<canonical>/...` の prefix 一致で行う。
//   `<merged_dir>/...` にあるファイルは "[merged]" 擬似プラグインに集約する
//   (hard link なので復元は可能だが、inode 判定は cross-platform で高コスト
//   なので初版では諦める)。
// - `self_ms` (子を含まない時間) を主指標にする。`--startuptime` の
//   `self+sourced` は require 連鎖が深い plugin で二重計上になるため。

use std::collections::HashMap;
use std::path::PathBuf;

/// `nvim --startuptime` 1 行分の「ファイル sourcing」エントリ。
/// event 行 (`--- NVIM STARTING ---` 等) は対象外。
#[derive(Debug, Clone, PartialEq)]
pub struct SourceEntry {
    /// sourcing / require 対象の絶対パス (forward slash 正規化済み)
    pub path: String,
    /// このファイル単体で消費した時間 (ms)
    pub self_ms: f64,
    /// 子 require を含めた時間 (ms)
    pub sourced_ms: f64,
}

/// 1 プラグイン分の集計結果。複数 run を平均した値。
#[derive(Debug, Clone)]
pub struct PluginStats {
    /// 表示名 (Plugin::display_name() or "[merged]" / "[runtime]" 等)
    pub name: String,
    /// 合計 self 時間 (ms)
    pub total_self_ms: f64,
    /// 合計 self+sourced 時間 (ms) — 参考値
    pub total_sourced_ms: f64,
    /// カウントされたファイル数
    pub file_count: usize,
    /// self_ms 降順の上位ファイル
    pub top_files: Vec<FileStat>,
    /// rvpm が管理するプラグインか (true: config.toml 由来、false: 擬似グループ)
    pub is_managed: bool,
    /// lazy プラグインか (擬似グループは false)
    pub lazy: bool,
}

/// プラグイン内の 1 ファイルの統計。
#[derive(Debug, Clone)]
pub struct FileStat {
    /// プラグインルートからの相対パス (forward slash 正規化)
    pub relative_path: String,
    pub self_ms: f64,
    pub sourced_ms: f64,
}

/// `rvpm profile` 実行結果の全体レポート。
#[derive(Debug, Clone)]
pub struct ProfileReport {
    /// 測定回数 (averaged)
    pub runs: usize,
    /// 全体の平均起動時間 (ms) = 各 run の最終 clock 値の平均
    pub total_startup_ms: f64,
    /// プラグイン (and 擬似グループ) 一覧。total_self_ms 降順。
    pub plugins: Vec<PluginStats>,
    /// nvim バイナリ情報 (取得できれば)
    pub nvim_version: Option<String>,
}

/// --startuptime 1 ファイル分の出力をパースして SourceEntry 列を返す。
///
/// 対象行の例:
/// ```text
/// 002.345  000.012  000.005: sourcing /path/to/file.lua
/// 002.456  000.015  000.008: require('foo')
/// ```
///
/// 非対象行 (event、ヘッダ、空行):
/// ```text
/// 000.008  000.008: --- NVIM STARTING ---
/// 000.110  000.102: event init
/// times in msec
/// ```
///
/// 判定基準: `:` より前にスペース区切りの数値が **3 つ** ある行のみ (source/require)。
/// 2 つだけの行は event なので skip。
pub fn parse_startuptime(content: &str) -> Vec<SourceEntry> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let Some(entry) = parse_startuptime_line(line) else {
            continue;
        };
        entries.push(entry);
    }
    entries
}

fn parse_startuptime_line(line: &str) -> Option<SourceEntry> {
    // `:` で前半 (数値列) と後半 (説明) を分ける。説明側に `:` を含むケースは
    // source/require 対象のパスに限られ (通常は Windows の `C:\...`)、その場合は
    // 最初の `:` で分けて前半が 3 数値になるかで判定する。
    let (head, tail) = line.split_once(':')?;
    let nums: Vec<f64> = head
        .split_whitespace()
        .filter_map(|s| s.parse::<f64>().ok())
        .collect();
    if nums.len() != 3 {
        return None;
    }
    let tail = tail.trim_start();
    let path = extract_source_path(tail)?;
    Some(SourceEntry {
        path: normalize_path(&path),
        self_ms: nums[2],
        sourced_ms: nums[1],
    })
}

/// tail 部分 (`: ` の右側) から対象パス or require 名を抽出する。
/// 形式:
///   - `sourcing /path/to/file.lua`
///   - `sourcing C:\path\to\file.lua`
///   - `require('vim.shared')` — require はファイル不明なのでここでは skip
fn extract_source_path(tail: &str) -> Option<String> {
    if let Some(rest) = tail.strip_prefix("sourcing ") {
        return Some(rest.trim().to_string());
    }
    // require() の場合はパス情報が無いので集計に使えない → None
    // event ("before startup", "inits 1", 等) も None
    None
}

/// 比較用にパスを正規化: backslash → forward slash。末尾スペースを除く。
fn normalize_path(p: &str) -> String {
    p.trim().replace('\\', "/")
}

/// プラグインの prefix 解決に必要な情報。
#[derive(Debug, Clone)]
pub struct PluginPathEntry {
    pub name: String,
    /// clone 先の絶対パス (forward slash 正規化済み)
    pub root: String,
    pub lazy: bool,
}

/// 擬似グループ名。
pub const GROUP_MERGED: &str = "[merged]";
pub const GROUP_RUNTIME: &str = "[runtime]";
pub const GROUP_LOADER: &str = "[rvpm loader]";
pub const GROUP_USER: &str = "[user config]";

/// SourceEntry 列 + プラグインパス情報から PluginStats を構築。
///
/// 単一 run 分の集計なので、平均化は呼び出し側で行う。
///
/// 帰属ロジック:
///   1. `<plugin.root>/...` にマッチ → そのプラグイン
///   2. `<merged_dir>/...`             → [merged]
///   3. `<loader_path>` 完全一致        → [rvpm loader]
///   4. `<user_config_root>/...`        → [user config]
///   5. それ以外                         → [runtime]
///
/// plugin の prefix は長い順に試す (深いパスが先にマッチするように) — 通常は
/// `<repos_dir>/<host>/<owner>/<repo>` 構造なので衝突しないが、`plugin.dst` で
/// 入れ子にできるケースを意識。
pub fn aggregate_single_run(
    entries: &[SourceEntry],
    plugins: &[PluginPathEntry],
    merged_dir: &str,
    loader_path: &str,
    user_config_root: &str,
) -> HashMap<String, PluginStats> {
    // Windows 由来の backslash パスと nvim の forward slash 出力が混在するので、
    // 比較前にすべて forward slash + lowercase 化して揃える必要がある。
    // 元の `name` 表示は保持したまま、内部比較用の key だけ正規化する。
    let normalized_plugins: Vec<(String, PluginPathEntry)> = plugins
        .iter()
        .map(|p| (normalize_path(&p.root), p.clone()))
        .collect();
    let mut sorted_plugins: Vec<&(String, PluginPathEntry)> = normalized_plugins.iter().collect();
    sorted_plugins.sort_by_key(|(root, _)| std::cmp::Reverse(root.len()));

    let merged = normalize_path(merged_dir);
    let loader = normalize_path(loader_path);
    let user_root = normalize_path(user_config_root);

    let mut stats: HashMap<String, PluginStats> = HashMap::new();

    for entry in entries {
        let (owner_name, is_managed, lazy, rel) =
            resolve_owner(&entry.path, &sorted_plugins, &merged, &loader, &user_root);

        let s = stats
            .entry(owner_name.clone())
            .or_insert_with(|| PluginStats {
                name: owner_name,
                total_self_ms: 0.0,
                total_sourced_ms: 0.0,
                file_count: 0,
                top_files: Vec::new(),
                is_managed,
                lazy,
            });
        s.total_self_ms += entry.self_ms;
        s.total_sourced_ms += entry.sourced_ms;
        s.file_count += 1;
        s.top_files.push(FileStat {
            relative_path: rel,
            self_ms: entry.self_ms,
            sourced_ms: entry.sourced_ms,
        });
    }

    // 各 PluginStats 内で top_files を self_ms 降順にソート
    for s in stats.values_mut() {
        s.top_files.sort_by(|a, b| {
            b.self_ms
                .partial_cmp(&a.self_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    stats
}

/// 指定パスの所有者 (plugin or 擬似グループ) を解決。
/// 返り値: (name, is_managed, lazy, relative_path)
fn resolve_owner(
    path: &str,
    sorted_plugins: &[&(String, PluginPathEntry)],
    merged: &str,
    loader: &str,
    user_root: &str,
) -> (String, bool, bool, String) {
    // case-insensitive prefix match — Windows でドライブレター / 正規化揺れがあっても拾う
    let path_lc = path.to_ascii_lowercase();

    for (root, p) in sorted_plugins {
        let root_lc = root.to_ascii_lowercase();
        if path_starts_with(&path_lc, &root_lc) {
            let rel = strip_prefix_with_sep(path, root);
            return (p.name.clone(), true, p.lazy, rel);
        }
    }

    let merged_lc = merged.to_ascii_lowercase();
    if path_starts_with(&path_lc, &merged_lc) {
        let rel = strip_prefix_with_sep(path, merged);
        return (GROUP_MERGED.to_string(), false, false, rel);
    }

    if path_lc == loader.to_ascii_lowercase() {
        return (
            GROUP_LOADER.to_string(),
            false,
            false,
            "loader.lua".to_string(),
        );
    }

    let user_lc = user_root.to_ascii_lowercase();
    if !user_root.is_empty() && path_starts_with(&path_lc, &user_lc) {
        let rel = strip_prefix_with_sep(path, user_root);
        return (GROUP_USER.to_string(), false, false, rel);
    }

    // 最後の segment (basename) を相対パスとして保持 — 見やすさ重視
    let basename = path.rsplit('/').next().unwrap_or(path).to_string();
    (GROUP_RUNTIME.to_string(), false, false, basename)
}

/// `path` が `prefix` で始まり、かつ prefix の直後が `/` or EOL であることを確認。
/// 単純な starts_with だと `/foo/barbaz` が prefix `/foo/bar` にマッチしてしまう。
fn path_starts_with(path: &str, prefix: &str) -> bool {
    if !path.starts_with(prefix) {
        return false;
    }
    let rest = &path[prefix.len()..];
    rest.is_empty() || rest.starts_with('/')
}

/// `path` から `prefix/` を除去した相対パス。prefix が末尾に `/` 無しでも対応。
fn strip_prefix_with_sep(path: &str, prefix: &str) -> String {
    let Some(rest) = path.strip_prefix(prefix) else {
        return path.to_string();
    };
    rest.trim_start_matches('/').to_string()
}

/// 複数 run の HashMap<String, PluginStats> を平均化して、ProfileReport 用の
/// Vec<PluginStats> (総 self 時間降順) に変換する。
///
/// 各 run で出現しないプラグインは 0 ms として平均に含める (= 分母は runs)。
pub fn average_stats(
    runs_stats: Vec<HashMap<String, PluginStats>>,
    runs: usize,
) -> Vec<PluginStats> {
    if runs == 0 {
        return Vec::new();
    }
    let mut merged: HashMap<String, PluginStats> = HashMap::new();

    for single in runs_stats {
        for (name, s) in single {
            let entry = merged.entry(name.clone()).or_insert_with(|| PluginStats {
                name: s.name.clone(),
                total_self_ms: 0.0,
                total_sourced_ms: 0.0,
                file_count: 0,
                top_files: Vec::new(),
                is_managed: s.is_managed,
                lazy: s.lazy,
            });
            entry.total_self_ms += s.total_self_ms;
            entry.total_sourced_ms += s.total_sourced_ms;
            // file_count は run 間で同じはずなので max を取る
            entry.file_count = entry.file_count.max(s.file_count);
            // top_files は最新 run のものを採用 (run 間で顔ぶれはほぼ同じ想定)
            if entry.top_files.is_empty() || s.top_files.len() > entry.top_files.len() {
                entry.top_files = s.top_files;
            }
        }
    }

    let mut out: Vec<PluginStats> = merged
        .into_values()
        .map(|mut s| {
            s.total_self_ms /= runs as f64;
            s.total_sourced_ms /= runs as f64;
            for f in s.top_files.iter_mut() {
                // top_files の時間も run 平均に近づけるため runs で割る
                // (実際にはその run 1 回分の値だが、視覚上プラグイン合計とズレるのを防ぐ)
                f.self_ms /= runs as f64;
                f.sourced_ms /= runs as f64;
            }
            s
        })
        .collect();

    out.sort_by(|a, b| {
        b.total_self_ms
            .partial_cmp(&a.total_self_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// --startuptime 出力の最終行から「全体起動時間」を推定する。
/// 最後の数値エントリの clock (= 最初の数値) を使う。
pub fn extract_total_ms(content: &str) -> f64 {
    let mut last: f64 = 0.0;
    for line in content.lines() {
        let Some((head, _)) = line.split_once(':') else {
            continue;
        };
        let nums: Vec<f64> = head
            .split_whitespace()
            .filter_map(|s| s.parse::<f64>().ok())
            .collect();
        if !nums.is_empty() {
            last = last.max(nums[0]);
        }
    }
    last
}

/// 1 回分の nvim startup を計測する。成功時は (startuptime 出力内容, 総起動 ms)。
/// nvim コマンド失敗時は Err。
pub async fn run_single_startuptime(extra_args: &[&str]) -> anyhow::Result<(String, f64)> {
    let tmp_path = std::env::temp_dir().join(format!(
        "rvpm-profile-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    let mut cmd = tokio::process::Command::new("nvim");
    cmd.arg("--headless")
        .arg("--startuptime")
        .arg(&tmp_path)
        .args(extra_args)
        .arg("+qa");

    // 30 秒 timeout — 通常は秒以下だが、何かの拍子で hang したときに
    // profile コマンド全体が固まらないよう安全弁を張る。
    let timeout = std::time::Duration::from_secs(30);
    let out_result = tokio::time::timeout(timeout, cmd.output()).await;

    match out_result {
        Ok(Ok(_out)) => {
            let content = std::fs::read_to_string(&tmp_path).unwrap_or_default();
            let _ = std::fs::remove_file(&tmp_path);
            if content.is_empty() {
                anyhow::bail!("nvim produced empty --startuptime output");
            }
            let total = extract_total_ms(&content);
            Ok((content, total))
        }
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(anyhow::anyhow!("failed to spawn nvim: {}", e))
        }
        Err(_) => {
            let _ = std::fs::remove_file(&tmp_path);
            anyhow::bail!("nvim --startuptime timed out after {:?}", timeout)
        }
    }
}

/// `nvim --version` の 1 行目を取得 (resilience: 取れなければ None)。
pub async fn probe_nvim_version() -> Option<String> {
    let timeout = std::time::Duration::from_secs(2);
    let cmd = tokio::process::Command::new("nvim")
        .arg("--version")
        .output();
    let out = tokio::time::timeout(timeout, cmd).await.ok()?.ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.lines().next().map(|s| s.trim().to_string())
}

/// N 回実行 → 平均して ProfileReport を組み立てる。
pub async fn run_profile(
    runs: usize,
    plugins: Vec<PluginPathEntry>,
    merged_dir: PathBuf,
    loader_path: PathBuf,
    user_config_root: PathBuf,
) -> anyhow::Result<ProfileReport> {
    if runs == 0 {
        anyhow::bail!("runs must be >= 1");
    }

    let merged_s = merged_dir.to_string_lossy().to_string();
    let loader_s = loader_path.to_string_lossy().to_string();
    let user_s = user_config_root.to_string_lossy().to_string();

    let mut totals = Vec::with_capacity(runs);
    let mut runs_stats = Vec::with_capacity(runs);

    for i in 0..runs {
        let (content, total) = run_single_startuptime(&[])
            .await
            .map_err(|e| anyhow::anyhow!("profile run {}/{} failed: {}", i + 1, runs, e))?;
        totals.push(total);
        let entries = parse_startuptime(&content);
        let stats = aggregate_single_run(&entries, &plugins, &merged_s, &loader_s, &user_s);
        runs_stats.push(stats);
    }

    let total_startup_ms = totals.iter().sum::<f64>() / runs as f64;
    let plugins_stats = average_stats(runs_stats, runs);

    let nvim_version = probe_nvim_version().await;

    Ok(ProfileReport {
        runs,
        total_startup_ms,
        plugins: plugins_stats,
        nvim_version,
    })
}

/// 擬似グループかどうか判定 (TUI の色分けに使う)。
pub fn is_group_name(name: &str) -> bool {
    matches!(
        name,
        GROUP_MERGED | GROUP_RUNTIME | GROUP_LOADER | GROUP_USER
    )
}

/// JSON 出力用に ProfileReport を serde_json::Value に変換。
pub fn report_to_json(report: &ProfileReport) -> serde_json::Value {
    serde_json::json!({
        "runs": report.runs,
        "total_startup_ms": report.total_startup_ms,
        "nvim_version": report.nvim_version,
        "plugins": report.plugins.iter().map(|p| serde_json::json!({
            "name": p.name,
            "total_self_ms": p.total_self_ms,
            "total_sourced_ms": p.total_sourced_ms,
            "file_count": p.file_count,
            "is_managed": p.is_managed,
            "lazy": p.lazy,
            "top_files": p.top_files.iter().map(|f| serde_json::json!({
                "path": f.relative_path,
                "self_ms": f.self_ms,
                "sourced_ms": f.sourced_ms,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_header_and_events() {
        let sample = "\
times in msec
 clock   self+sourced   self:  sourced script
 clock   elapsed:              other lines

000.008  000.008: --- NVIM STARTING ---
000.110  000.102: event init
";
        let entries = parse_startuptime(sample);
        assert!(entries.is_empty(), "event lines should be ignored");
    }

    #[test]
    fn parse_extracts_sourcing_entries() {
        let sample = "\
002.345  000.012  000.005: sourcing /home/me/.local/share/nvim/plugin/foo.lua
002.456  000.015  000.008: sourcing C:\\Users\\me\\plugins\\bar.vim
002.500  000.001  000.001: require('vim.shared')
";
        let entries = parse_startuptime(sample);
        assert_eq!(entries.len(), 2, "require lines are skipped");
        assert_eq!(entries[0].self_ms, 0.005);
        assert_eq!(entries[0].sourced_ms, 0.012);
        assert_eq!(entries[0].path, "/home/me/.local/share/nvim/plugin/foo.lua");
        assert_eq!(entries[1].self_ms, 0.008);
        // backslashes normalized
        assert_eq!(entries[1].path, "C:/Users/me/plugins/bar.vim");
    }

    #[test]
    fn parse_handles_windows_drive_colon_in_path() {
        // Windows のパスに含まれる `C:` の `:` が誤解析されないか
        let line = "010.234  000.050  000.042: sourcing C:/foo/bar/baz.lua";
        let entry = parse_startuptime_line(line).expect("should parse");
        assert_eq!(entry.path, "C:/foo/bar/baz.lua");
        assert_eq!(entry.self_ms, 0.042);
    }

    #[test]
    fn extract_total_returns_last_clock() {
        let sample = "\
000.100  000.100: event a
002.345  000.012  000.005: sourcing /foo.lua
005.678  000.015  000.008: sourcing /bar.lua
";
        assert!((extract_total_ms(sample) - 5.678).abs() < 1e-6);
    }

    fn plugin(name: &str, root: &str, lazy: bool) -> PluginPathEntry {
        PluginPathEntry {
            name: name.to_string(),
            root: root.to_string(),
            lazy,
        }
    }

    #[test]
    fn aggregate_attributes_file_to_matching_plugin() {
        let entries = vec![
            SourceEntry {
                path: "/cache/repos/github.com/owner/foo/plugin/foo.lua".into(),
                self_ms: 10.0,
                sourced_ms: 12.0,
            },
            SourceEntry {
                path: "/cache/repos/github.com/owner/foo/lua/foo/init.lua".into(),
                self_ms: 5.0,
                sourced_ms: 5.0,
            },
        ];
        let plugins = vec![plugin("foo", "/cache/repos/github.com/owner/foo", false)];
        let stats = aggregate_single_run(
            &entries,
            &plugins,
            "/cache/merged",
            "/cache/loader.lua",
            "/config",
        );
        let foo = stats.get("foo").expect("foo should exist");
        assert_eq!(foo.file_count, 2);
        assert!((foo.total_self_ms - 15.0).abs() < 1e-6);
        assert!(foo.is_managed);
        // top_files は self 降順
        assert_eq!(foo.top_files[0].relative_path, "plugin/foo.lua");
        assert_eq!(foo.top_files[1].relative_path, "lua/foo/init.lua");
    }

    #[test]
    fn aggregate_buckets_merged_and_runtime() {
        let entries = vec![
            SourceEntry {
                path: "/cache/merged/plugin/common.lua".into(),
                self_ms: 3.0,
                sourced_ms: 3.0,
            },
            SourceEntry {
                path: "/usr/share/nvim/runtime/plugin/foo.vim".into(),
                self_ms: 1.0,
                sourced_ms: 1.0,
            },
            SourceEntry {
                path: "/cache/loader.lua".into(),
                self_ms: 2.0,
                sourced_ms: 2.0,
            },
        ];
        let plugins = vec![];
        let stats = aggregate_single_run(
            &entries,
            &plugins,
            "/cache/merged",
            "/cache/loader.lua",
            "/config",
        );
        assert!(stats.contains_key(GROUP_MERGED));
        assert!(stats.contains_key(GROUP_RUNTIME));
        assert!(stats.contains_key(GROUP_LOADER));
        assert!((stats.get(GROUP_MERGED).unwrap().total_self_ms - 3.0).abs() < 1e-6);
    }

    #[test]
    fn aggregate_prefers_deeper_plugin_path() {
        // プラグイン root が入れ子になったケースで、長い方が優先されるか
        let entries = vec![SourceEntry {
            path: "/plugins/outer/inner/plugin/x.lua".into(),
            self_ms: 4.0,
            sourced_ms: 4.0,
        }];
        let plugins = vec![
            plugin("outer", "/plugins/outer", false),
            plugin("inner", "/plugins/outer/inner", false),
        ];
        let stats = aggregate_single_run(
            &entries,
            &plugins,
            "/cache/merged",
            "/cache/loader.lua",
            "/config",
        );
        assert!(stats.contains_key("inner"));
        assert!(!stats.contains_key("outer"));
    }

    #[test]
    fn path_starts_with_rejects_partial_segment() {
        // `/foo/barbaz` が prefix `/foo/bar` にマッチしないこと
        assert!(!path_starts_with("/foo/barbaz/x", "/foo/bar"));
        assert!(path_starts_with("/foo/bar/x", "/foo/bar"));
        assert!(path_starts_with("/foo/bar", "/foo/bar"));
    }

    #[test]
    fn average_divides_by_runs_and_sorts_desc() {
        let mut run1 = HashMap::new();
        run1.insert(
            "a".to_string(),
            PluginStats {
                name: "a".into(),
                total_self_ms: 20.0,
                total_sourced_ms: 25.0,
                file_count: 2,
                top_files: vec![FileStat {
                    relative_path: "plugin/a.lua".into(),
                    self_ms: 20.0,
                    sourced_ms: 25.0,
                }],
                is_managed: true,
                lazy: false,
            },
        );
        run1.insert(
            "b".to_string(),
            PluginStats {
                name: "b".into(),
                total_self_ms: 40.0,
                total_sourced_ms: 40.0,
                file_count: 1,
                top_files: vec![],
                is_managed: true,
                lazy: false,
            },
        );
        let mut run2 = HashMap::new();
        run2.insert(
            "a".to_string(),
            PluginStats {
                name: "a".into(),
                total_self_ms: 10.0,
                total_sourced_ms: 15.0,
                file_count: 2,
                top_files: vec![],
                is_managed: true,
                lazy: false,
            },
        );
        // b は run2 には無い (0 ms として扱う)

        let avg = average_stats(vec![run1, run2], 2);
        // 期待: a = (20+10)/2 = 15, b = 40/2 = 20 → b が先
        assert_eq!(avg[0].name, "b");
        assert!((avg[0].total_self_ms - 20.0).abs() < 1e-6);
        assert_eq!(avg[1].name, "a");
        assert!((avg[1].total_self_ms - 15.0).abs() < 1e-6);
    }

    #[test]
    fn extract_source_path_rejects_require_lines() {
        assert!(extract_source_path("require('foo')").is_none());
        assert_eq!(
            extract_source_path("sourcing /foo/bar.lua"),
            Some("/foo/bar.lua".to_string())
        );
    }
}
