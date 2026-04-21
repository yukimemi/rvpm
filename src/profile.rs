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
    /// Phase 4 (per-plugin init.lua) 所要時間 (ms)。instrumentation 有効時のみ。
    pub init_ms: f64,
    /// Phase 6 (eager main load) 所要時間 (ms)。lazy プラグインは 0。
    pub load_ms: f64,
    /// Phase 7 (lazy trigger 登録) 所要時間 (ms)。eager は 0。
    pub trig_ms: f64,
}

/// プラグイン内の 1 ファイルの統計。
#[derive(Debug, Clone)]
pub struct FileStat {
    /// プラグインルートからの相対パス (forward slash 正規化)
    pub relative_path: String,
    pub self_ms: f64,
    pub sourced_ms: f64,
}

/// 1 フェーズ分の所要時間 (平均値)。
#[derive(Debug, Clone, Default)]
pub struct PhaseTime {
    /// phase 名 ("phase-3" / "phase-4" / ... / "phase-9")
    pub name: String,
    /// 所要時間 (ms、平均)
    pub duration_ms: f64,
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
    /// instrumented run から得た phase タイムライン (None なら profile_mode OFF)。
    pub phase_timeline: Option<Vec<PhaseTime>>,
    /// --no-merge で計測したか (UI で注意表示するため)
    pub no_merge: bool,
    /// --no-instrument モードで計測したか (phase_timeline が常に None)
    pub no_instrument: bool,
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
///   4. `<user_config_roots[i]>/...`    → [user config] (rvpm と Neovim 両方)
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
    user_config_roots: &[String],
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
    let user_roots: Vec<String> = user_config_roots
        .iter()
        .map(|s| normalize_path(s))
        .filter(|s| !s.is_empty())
        .collect();

    let mut stats: HashMap<String, PluginStats> = HashMap::new();

    for entry in entries {
        let (owner_name, is_managed, lazy, rel) =
            resolve_owner(&entry.path, &sorted_plugins, &merged, &loader, &user_roots);

        let s = stats
            .entry(owner_name.clone())
            .or_insert_with(|| PluginStats {
                name: owner_name,
                total_self_ms: 0.0,
                total_sourced_ms: 0.0,
                file_count: 0,
                top_files: Vec::new(),
                is_managed,
                init_ms: 0.0,
                load_ms: 0.0,
                trig_ms: 0.0,
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
    user_roots: &[String],
) -> (String, bool, bool, String) {
    // case-insensitive prefix match — Windows でドライブレター / 正規化揺れがあっても拾う
    let path_lc = path.to_ascii_lowercase();

    for (root, p) in sorted_plugins {
        let root_lc = root.to_ascii_lowercase();
        if path_starts_with(&path_lc, &root_lc) {
            let rel = strip_prefix_case_insensitive(path, root);
            return (p.name.clone(), true, p.lazy, rel);
        }
    }

    let merged_lc = merged.to_ascii_lowercase();
    if path_starts_with(&path_lc, &merged_lc) {
        let rel = strip_prefix_case_insensitive(path, merged);
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

    // 複数の user config root を長い順に試す (Neovim の ~/.config/nvim と rvpm の両方)
    let mut sorted_user_roots: Vec<&String> = user_roots.iter().collect();
    sorted_user_roots.sort_by_key(|r| std::cmp::Reverse(r.len()));
    for user_root in sorted_user_roots {
        let user_lc = user_root.to_ascii_lowercase();
        if path_starts_with(&path_lc, &user_lc) {
            let rel = strip_prefix_case_insensitive(path, user_root);
            return (GROUP_USER.to_string(), false, false, rel);
        }
    }

    // 最後の segment (basename) を相対パスとして保持 — 見やすさ重視
    let basename = path.rsplit('/').next().unwrap_or(path).to_string();
    (GROUP_RUNTIME.to_string(), false, false, basename)
}

/// `path` が `prefix` で始まり、かつ prefix の直後が `/` or EOL であることを確認。
/// 単純な starts_with だと `/foo/barbaz` が prefix `/foo/bar` にマッチしてしまう。
///
/// `prefix` に末尾 `/` が含まれていれば、その時点でセグメント境界が保証されている
/// ので追加チェック不要。そうでないとき `path[prefix.len()..]` が `/` 区切りか
/// EOL であるかを確認する。
fn path_starts_with(path: &str, prefix: &str) -> bool {
    if !path.starts_with(prefix) {
        return false;
    }
    if prefix.ends_with('/') {
        return true;
    }
    let rest = &path[prefix.len()..];
    rest.is_empty() || rest.starts_with('/')
}

/// `path` から `prefix/` を除去した相対パス (case-insensitive 版)。
///
/// 呼び出し元は path_starts_with で小文字化した比較で一致確認済み前提。
/// prefix.len() バイト分を slice する (ASCII 前提) ことで、大文字小文字の違いで
/// strip_prefix が失敗して path 丸ごと返ってしまうのを防ぐ。ASCII 以外が prefix
/// に含まれるケースは rvpm のパス生成経路では発生しない。
fn strip_prefix_case_insensitive(path: &str, prefix: &str) -> String {
    if path.len() < prefix.len() {
        return path.to_string();
    }
    let rest = &path[prefix.len()..];
    rest.trim_start_matches('/').to_string()
}

/// 複数 run の HashMap<String, PluginStats> を平均化して、ProfileReport 用の
/// Vec<PluginStats> (総 self 時間降順) に変換する。
///
/// 各 run で出現しないプラグインは 0 ms として平均に含める (= 分母は runs)。
///
/// top_files は plugin 毎に `HashMap<path, 累積 (self_sum, sourced_sum)>` として
/// run 間で累積してから平均化する。単一 run の top_files を丸ごと使って後で割る
/// 方式だと「その 1 回に出た顔ぶれだけ」で、しかも 1 回分の時間を runs で割る分
/// 過小評価になる問題があった。
pub fn average_stats(
    runs_stats: Vec<HashMap<String, PluginStats>>,
    runs: usize,
) -> Vec<PluginStats> {
    if runs == 0 {
        return Vec::new();
    }
    let mut merged: HashMap<String, PluginStats> = HashMap::new();
    // plugin name → { file relative_path → (self_sum, sourced_sum) }
    let mut files_acc: HashMap<String, HashMap<String, (f64, f64)>> = HashMap::new();

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
                init_ms: 0.0,
                load_ms: 0.0,
                trig_ms: 0.0,
            });
            entry.total_self_ms += s.total_self_ms;
            entry.total_sourced_ms += s.total_sourced_ms;
            entry.init_ms += s.init_ms;
            entry.load_ms += s.load_ms;
            entry.trig_ms += s.trig_ms;
            // file_count は run 間で同じはずなので max を取る
            entry.file_count = entry.file_count.max(s.file_count);
            // ファイル単位で累積する
            let file_map = files_acc.entry(name).or_default();
            for f in &s.top_files {
                let e = file_map
                    .entry(f.relative_path.clone())
                    .or_insert((0.0, 0.0));
                e.0 += f.self_ms;
                e.1 += f.sourced_ms;
            }
        }
    }

    let mut out: Vec<PluginStats> = merged
        .into_iter()
        .map(|(name, mut s)| {
            s.total_self_ms /= runs as f64;
            s.total_sourced_ms /= runs as f64;
            s.init_ms /= runs as f64;
            s.load_ms /= runs as f64;
            s.trig_ms /= runs as f64;
            if let Some(file_map) = files_acc.remove(&name) {
                let mut files: Vec<FileStat> = file_map
                    .into_iter()
                    .map(|(path, (self_sum, sourced_sum))| FileStat {
                        relative_path: path,
                        self_ms: self_sum / runs as f64,
                        sourced_ms: sourced_sum / runs as f64,
                    })
                    .collect();
                files.sort_by(|a, b| {
                    b.self_ms
                        .partial_cmp(&a.self_ms)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                s.top_files = files;
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

/// `rvpm profile` instrumentation 由来の marker event。
/// `<marker_dir>/<name>.vim` を source した行の clock 値を記録する。
#[derive(Debug, Clone, PartialEq)]
pub struct MarkerEvent {
    /// event 名 (`phase-3-begin`, `init-telescope-end` 等、.vim 拡張子除去済み)
    pub name: String,
    /// sourcing 時の clock 値 (ms)
    pub clock_ms: f64,
}

/// --startuptime 出力から marker event を抽出する。
///
/// `sourcing <marker_dir>/<name>.vim` という形の行を検出し、
/// event 名 (拡張子除く) と clock 値を取り出す。
/// marker_dir は forward-slash 正規化済みの絶対パス前提。
///
/// 境界チェックは `path_starts_with` を使い、`/tmp/markers` が `/tmp/markers-old/...`
/// に誤マッチしないようセグメント区切りまで揃えて比較する。
pub fn parse_marker_events(content: &str, marker_dir_normalized: &str) -> Vec<MarkerEvent> {
    let prefix = normalize_path(marker_dir_normalized)
        .trim_end_matches('/')
        .to_string();
    let prefix_lc = prefix.to_ascii_lowercase();
    let mut events = Vec::new();
    for line in content.lines() {
        let Some((head, tail)) = line.split_once(':') else {
            continue;
        };
        let nums: Vec<f64> = head
            .split_whitespace()
            .filter_map(|s| s.parse::<f64>().ok())
            .collect();
        if nums.len() != 3 {
            continue;
        }
        let Some(rest) = tail.trim_start().strip_prefix("sourcing ") else {
            continue;
        };
        let path = normalize_path(rest.trim());
        let path_lc = path.to_ascii_lowercase();
        if !path_starts_with(&path_lc, &prefix_lc) {
            continue;
        }
        let rest_after = &path[prefix.len()..];
        let rest_after = rest_after.trim_start_matches('/');
        // `.vim` 拡張子を除いて event 名として取り出す
        let name = rest_after.trim_end_matches(".vim").to_string();
        if name.is_empty() {
            continue;
        }
        events.push(MarkerEvent {
            name,
            clock_ms: nums[0],
        });
    }
    events
}

/// phase-<N>-begin / phase-<N>-end のペアから各 phase の所要時間を計算する。
///
/// 対応する begin/end が両方見つかった phase のみ結果に含める。
/// 順序通りに (phase-3, phase-4, ..., phase-9) で並べる。
pub fn compute_phase_times(events: &[MarkerEvent]) -> Vec<PhaseTime> {
    use std::collections::HashMap;
    let mut begins: HashMap<&str, f64> = HashMap::new();
    let mut ends: HashMap<&str, f64> = HashMap::new();
    for e in events {
        if let Some(phase) = e.name.strip_suffix("-begin") {
            begins.insert(phase, e.clock_ms);
        } else if let Some(phase) = e.name.strip_suffix("-end") {
            ends.insert(phase, e.clock_ms);
        }
    }
    let order = [
        "phase-3", "phase-4", "phase-5", "phase-6", "phase-7", "phase-9",
    ];
    let mut out = Vec::new();
    for phase in order {
        if let (Some(b), Some(e)) = (begins.get(phase), ends.get(phase)) {
            out.push(PhaseTime {
                name: phase.to_string(),
                duration_ms: (e - b).max(0.0),
            });
        }
    }
    out
}

/// per-plugin の init-<safe>-begin/end と trig-<safe>-begin/end から
/// (init_ms, trig_ms) のマップを組み立てる。サニタイズ前の元の表示名は
/// main.rs 側で逆引きして合わせる (loader.rs::sanitize_name は `_` に置換する規則)。
///
/// 返り値の key は sanitize 済みの safe 名 — 呼び出し側で同じ規則で plugin.name
/// を正規化して lookup する。
pub fn compute_per_plugin_phase_times(
    events: &[MarkerEvent],
) -> std::collections::HashMap<String, (f64, f64)> {
    use std::collections::HashMap;
    let mut init_begin: HashMap<String, f64> = HashMap::new();
    let mut init_end: HashMap<String, f64> = HashMap::new();
    let mut trig_begin: HashMap<String, f64> = HashMap::new();
    let mut trig_end: HashMap<String, f64> = HashMap::new();
    for e in events {
        if let Some(rest) = e.name.strip_prefix("init-") {
            if let Some(name) = rest.strip_suffix("-begin") {
                init_begin.insert(name.to_string(), e.clock_ms);
            } else if let Some(name) = rest.strip_suffix("-end") {
                init_end.insert(name.to_string(), e.clock_ms);
            }
        } else if let Some(rest) = e.name.strip_prefix("trig-") {
            if let Some(name) = rest.strip_suffix("-begin") {
                trig_begin.insert(name.to_string(), e.clock_ms);
            } else if let Some(name) = rest.strip_suffix("-end") {
                trig_end.insert(name.to_string(), e.clock_ms);
            }
        }
    }
    let mut out: HashMap<String, (f64, f64)> = HashMap::new();
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    names.extend(init_begin.keys().cloned());
    names.extend(trig_begin.keys().cloned());
    for name in names {
        let i = match (init_begin.get(&name), init_end.get(&name)) {
            (Some(b), Some(e)) => (e - b).max(0.0),
            _ => 0.0,
        };
        let t = match (trig_begin.get(&name), trig_end.get(&name)) {
            (Some(b), Some(e)) => (e - b).max(0.0),
            _ => 0.0,
        };
        out.insert(name, (i, t));
    }
    out
}

/// 複数 run の phase timeline を平均化。phase 名は決まっているので順序は保てる。
pub fn average_phase_timelines(timelines: Vec<Vec<PhaseTime>>) -> Vec<PhaseTime> {
    use std::collections::HashMap;
    if timelines.is_empty() {
        return Vec::new();
    }
    let runs = timelines.len() as f64;
    let mut acc: HashMap<String, f64> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for t in &timelines {
        for p in t {
            if !acc.contains_key(&p.name) {
                order.push(p.name.clone());
            }
            *acc.entry(p.name.clone()).or_insert(0.0) += p.duration_ms;
        }
    }
    order
        .into_iter()
        .map(|name| {
            let total = acc.get(&name).copied().unwrap_or(0.0);
            PhaseTime {
                name,
                duration_ms: total / runs,
            }
        })
        .collect()
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
/// nvim コマンド失敗時 (spawn 失敗 / 非 0 exit / timeout) は Err。
///
/// 一時ファイルは `tempfile::NamedTempFile` で取る — `Drop` で自動削除されるので、
/// panic / 早期 return / timeout 時にも確実にクリーンアップされる。
pub async fn run_single_startuptime(extra_args: &[&str]) -> anyhow::Result<(String, f64)> {
    let tmp = tempfile::Builder::new()
        .prefix("rvpm-profile-")
        .suffix(".log")
        .tempfile()
        .map_err(|e| anyhow::anyhow!("failed to create startuptime tempfile: {}", e))?;
    let tmp_path = tmp.path().to_path_buf();

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
        Ok(Ok(out)) => {
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                anyhow::bail!(
                    "nvim exited with {} (stderr: {})",
                    out.status,
                    stderr.trim()
                );
            }
            let content = std::fs::read_to_string(&tmp_path).unwrap_or_default();
            if content.is_empty() {
                anyhow::bail!("nvim produced empty --startuptime output");
            }
            let total = extract_total_ms(&content);
            // tmp は drop で自動削除される
            drop(tmp);
            Ok((content, total))
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("failed to spawn nvim: {}", e)),
        Err(_) => {
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

/// `rvpm profile` 1 回分の実行パラメータ。
/// main.rs の run_profile から渡される (loader.lua の swap はそちら側で済んでいる前提)。
pub struct ProfileRunConfig {
    pub runs: usize,
    pub plugins: Vec<PluginPathEntry>,
    pub merged_dir: PathBuf,
    pub loader_path: PathBuf,
    /// rvpm 側と Neovim 側の config ディレクトリ。両方を [user config] 擬似
    /// グループの帰属先として扱う (Neovim の init.lua が [runtime] に落ちないように)。
    pub user_config_roots: Vec<PathBuf>,
    /// instrumentation 有効時の marker dir (空なら phase 分解をスキップ)。
    pub marker_dir: Option<PathBuf>,
    pub no_merge: bool,
    pub no_instrument: bool,
}

/// N 回実行 → 平均して ProfileReport を組み立てる。
pub async fn run_profile(cfg: ProfileRunConfig) -> anyhow::Result<ProfileReport> {
    if cfg.runs == 0 {
        anyhow::bail!("runs must be >= 1");
    }

    let merged_s = cfg.merged_dir.to_string_lossy().to_string();
    let loader_s = cfg.loader_path.to_string_lossy().to_string();
    let user_s: Vec<String> = cfg
        .user_config_roots
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let marker_s = cfg
        .marker_dir
        .as_ref()
        .map(|p| normalize_path(&p.to_string_lossy()));

    let mut totals = Vec::with_capacity(cfg.runs);
    let mut runs_stats = Vec::with_capacity(cfg.runs);
    let mut phase_timelines: Vec<Vec<PhaseTime>> = Vec::new();

    for i in 0..cfg.runs {
        let (content, total) = run_single_startuptime(&[])
            .await
            .map_err(|e| anyhow::anyhow!("profile run {}/{} failed: {}", i + 1, cfg.runs, e))?;
        totals.push(total);
        let entries = parse_startuptime(&content);
        let mut stats = aggregate_single_run(
            &entries,
            &cfg.plugins,
            &merged_s,
            &loader_s,
            user_s.as_slice(),
        );

        // eager プラグインの load_ms は instrumentation の有無に関わらず
        // sourcing 合計で近似できるので、marker_s != None 条件の外で先に書く。
        for s in stats.values_mut() {
            if s.is_managed && !s.lazy {
                s.load_ms = s.total_self_ms;
            }
        }

        // phase / per-plugin marker を parse できれば stats に反映
        if let Some(mdir) = &marker_s {
            let markers = parse_marker_events(&content, mdir);
            let phases = compute_phase_times(&markers);
            let per_plugin = compute_per_plugin_phase_times(&markers);

            // lazy プラグインは sourcing 行を出さないので stats に entry が無い。
            // marker で init/trig が取れたプラグインの空エントリを事前に作る。
            for plugin in &cfg.plugins {
                let safe = crate::loader::sanitize_name(&plugin.name);
                if per_plugin.contains_key(&safe) && !stats.contains_key(&plugin.name) {
                    stats.insert(
                        plugin.name.clone(),
                        PluginStats {
                            name: plugin.name.clone(),
                            total_self_ms: 0.0,
                            total_sourced_ms: 0.0,
                            file_count: 0,
                            top_files: Vec::new(),
                            is_managed: true,
                            lazy: plugin.lazy,
                            init_ms: 0.0,
                            load_ms: 0.0,
                            trig_ms: 0.0,
                        },
                    );
                }
            }

            // `cfg.plugins.iter().find` を掛けると O(N²) になるので、s.name を
            // 直接 sanitize して per_plugin から引くだけの O(N) に留める。
            // PluginStats は既に is_managed / lazy を保持済みなので追加の lookup は不要。
            for s in stats.values_mut() {
                if !s.is_managed {
                    continue;
                }
                let safe = crate::loader::sanitize_name(&s.name);
                if let Some((init, trig)) = per_plugin.get(&safe) {
                    s.init_ms = *init;
                    s.trig_ms = *trig;
                }
            }
            phase_timelines.push(phases);
        }

        runs_stats.push(stats);
    }

    let total_startup_ms = totals.iter().sum::<f64>() / cfg.runs as f64;
    let plugins_stats = average_stats(runs_stats, cfg.runs);

    let phase_timeline = if phase_timelines.is_empty() {
        None
    } else {
        Some(average_phase_timelines(phase_timelines))
    };

    let nvim_version = probe_nvim_version().await;

    Ok(ProfileReport {
        runs: cfg.runs,
        total_startup_ms,
        plugins: plugins_stats,
        nvim_version,
        phase_timeline,
        no_merge: cfg.no_merge,
        no_instrument: cfg.no_instrument,
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
        "no_merge": report.no_merge,
        "no_instrument": report.no_instrument,
        "phase_timeline": report.phase_timeline.as_ref().map(|pts| pts.iter().map(|p| serde_json::json!({
            "name": p.name,
            "duration_ms": p.duration_ms,
        })).collect::<Vec<_>>()),
        "plugins": report.plugins.iter().map(|p| serde_json::json!({
            "name": p.name,
            "total_self_ms": p.total_self_ms,
            "total_sourced_ms": p.total_sourced_ms,
            "init_ms": p.init_ms,
            "load_ms": p.load_ms,
            "trig_ms": p.trig_ms,
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
            &["/config".to_string()],
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
            &["/config".to_string()],
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
            &["/config".to_string()],
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
                init_ms: 0.0,
                load_ms: 0.0,
                trig_ms: 0.0,
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
                init_ms: 0.0,
                load_ms: 0.0,
                trig_ms: 0.0,
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
                init_ms: 0.0,
                load_ms: 0.0,
                trig_ms: 0.0,
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

    #[test]
    fn parse_marker_events_extracts_phase_markers() {
        let content = "\
010.100  000.005  000.005: sourcing /tmp/markers/phase-3-begin.vim
010.500  000.008  000.008: sourcing /tmp/markers/phase-3-end.vim
011.200  000.003  000.003: sourcing /tmp/markers/init-telescope-begin.vim
011.800  000.012  000.012: sourcing /tmp/markers/init-telescope-end.vim
020.000  000.010  000.010: sourcing /some/other/plugin.lua
";
        let events = parse_marker_events(content, "/tmp/markers");
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].name, "phase-3-begin");
        assert_eq!(events[0].clock_ms, 10.100);
        assert_eq!(events[3].name, "init-telescope-end");
        assert!((events[3].clock_ms - 11.800).abs() < 1e-6);
    }

    #[test]
    fn compute_phase_times_pairs_begin_end() {
        let events = vec![
            MarkerEvent {
                name: "phase-3-begin".into(),
                clock_ms: 10.0,
            },
            MarkerEvent {
                name: "phase-3-end".into(),
                clock_ms: 15.0,
            },
            MarkerEvent {
                name: "phase-6-begin".into(),
                clock_ms: 20.0,
            },
            MarkerEvent {
                name: "phase-6-end".into(),
                clock_ms: 100.0,
            },
        ];
        let phases = compute_phase_times(&events);
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].name, "phase-3");
        assert_eq!(phases[0].duration_ms, 5.0);
        assert_eq!(phases[1].name, "phase-6");
        assert_eq!(phases[1].duration_ms, 80.0);
    }

    #[test]
    fn compute_phase_times_skips_unpaired() {
        // phase-4 に begin しか無い場合 (壊れた instrumentation) は skip
        let events = vec![
            MarkerEvent {
                name: "phase-3-begin".into(),
                clock_ms: 10.0,
            },
            MarkerEvent {
                name: "phase-3-end".into(),
                clock_ms: 12.0,
            },
            MarkerEvent {
                name: "phase-4-begin".into(),
                clock_ms: 13.0,
            },
            // phase-4-end 欠落
        ];
        let phases = compute_phase_times(&events);
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].name, "phase-3");
    }

    #[test]
    fn compute_per_plugin_phase_times_extracts_init_and_trig() {
        let events = vec![
            MarkerEvent {
                name: "init-alpha-begin".into(),
                clock_ms: 10.0,
            },
            MarkerEvent {
                name: "init-alpha-end".into(),
                clock_ms: 10.5,
            },
            MarkerEvent {
                name: "trig-beta-begin".into(),
                clock_ms: 20.0,
            },
            MarkerEvent {
                name: "trig-beta-end".into(),
                clock_ms: 20.3,
            },
        ];
        let pp = compute_per_plugin_phase_times(&events);
        assert!((pp["alpha"].0 - 0.5).abs() < 1e-6, "alpha init_ms");
        assert_eq!(pp["alpha"].1, 0.0, "alpha has no trig");
        assert!((pp["beta"].1 - 0.3).abs() < 1e-6, "beta trig_ms");
        assert_eq!(pp["beta"].0, 0.0, "beta has no init");
    }

    #[test]
    fn aggregate_accepts_multiple_user_config_roots() {
        // Neovim の ~/.config/nvim と rvpm の ~/.config/rvpm の両方で [user config] にする
        let entries = vec![
            SourceEntry {
                path: "/home/me/.config/nvim/init.lua".into(),
                self_ms: 5.0,
                sourced_ms: 5.0,
            },
            SourceEntry {
                path: "/home/me/.config/rvpm/nvim/before.lua".into(),
                self_ms: 2.0,
                sourced_ms: 2.0,
            },
        ];
        let stats = aggregate_single_run(
            &entries,
            &[],
            "/cache/merged",
            "/cache/loader.lua",
            &[
                "/home/me/.config/nvim".to_string(),
                "/home/me/.config/rvpm/nvim".to_string(),
            ],
        );
        let u = stats
            .get(GROUP_USER)
            .expect("should bucket under [user config]");
        assert_eq!(u.file_count, 2);
        assert!(!stats.contains_key(GROUP_RUNTIME));
    }

    #[test]
    fn aggregate_strips_prefix_case_insensitive_on_windows_paths() {
        // Windows drive letter を大文字で emit、plugin root を小文字で emit する実データ
        // を想定。以前は `/c:/users/...` (rel に prefix 丸ごと残る) になっていた。
        let entries = vec![SourceEntry {
            path: "C:/Users/me/plugin/foo.lua".into(),
            self_ms: 1.0,
            sourced_ms: 1.0,
        }];
        let plugins = vec![PluginPathEntry {
            name: "foo".into(),
            root: "c:/users/me".into(),
            lazy: false,
        }];
        let stats = aggregate_single_run(
            &entries,
            &plugins,
            "/cache/merged",
            "/cache/loader.lua",
            &[],
        );
        let foo = stats.get("foo").expect("should match case-insensitive");
        assert_eq!(foo.top_files[0].relative_path, "plugin/foo.lua");
    }

    #[test]
    fn average_stats_aggregates_top_files_across_runs() {
        // 同じ plugin が 2 runs に亘って登場し、同じ file を両方で source したとき、
        // top_files の self_ms が単一 run 丸ごとじゃなく、平均 (合計 / runs) になるか。
        let make_stats = |self_ms: f64| {
            let mut m = HashMap::new();
            m.insert(
                "plug".to_string(),
                PluginStats {
                    name: "plug".into(),
                    total_self_ms: self_ms,
                    total_sourced_ms: self_ms,
                    file_count: 1,
                    top_files: vec![FileStat {
                        relative_path: "plugin/x.lua".into(),
                        self_ms,
                        sourced_ms: self_ms,
                    }],
                    is_managed: true,
                    lazy: false,
                    init_ms: 0.0,
                    load_ms: 0.0,
                    trig_ms: 0.0,
                },
            );
            m
        };
        let avg = average_stats(vec![make_stats(10.0), make_stats(20.0)], 2);
        assert_eq!(avg.len(), 1);
        let plug = &avg[0];
        assert!((plug.total_self_ms - 15.0).abs() < 1e-6);
        assert_eq!(plug.top_files.len(), 1);
        // (10 + 20) / 2 = 15 — 以前のバグでは 10/2 = 5 になっていた
        assert!(
            (plug.top_files[0].self_ms - 15.0).abs() < 1e-6,
            "got {}",
            plug.top_files[0].self_ms
        );
    }

    #[test]
    fn average_phase_timelines_handles_multiple_runs() {
        let r1 = vec![
            PhaseTime {
                name: "phase-3".into(),
                duration_ms: 4.0,
            },
            PhaseTime {
                name: "phase-6".into(),
                duration_ms: 100.0,
            },
        ];
        let r2 = vec![
            PhaseTime {
                name: "phase-3".into(),
                duration_ms: 6.0,
            },
            PhaseTime {
                name: "phase-6".into(),
                duration_ms: 80.0,
            },
        ];
        let avg = average_phase_timelines(vec![r1, r2]);
        assert_eq!(avg.len(), 2);
        assert_eq!(avg[0].name, "phase-3");
        assert!((avg[0].duration_ms - 5.0).abs() < 1e-6);
        assert_eq!(avg[1].name, "phase-6");
        assert!((avg[1].duration_ms - 90.0).abs() < 1e-6);
    }
}
