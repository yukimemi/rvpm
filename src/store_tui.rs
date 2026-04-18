use crate::store::GitHubRepo;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, TableState, Wrap},
};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Readme,
}

pub struct StoreTuiState {
    pub plugins: Vec<GitHubRepo>,
    pub table_state: TableState,
    /// ローカルインクリメンタル検索の入力モード (`/` キー)
    pub search_mode: bool,
    /// GitHub API 検索の入力モード (`S` キー) — 旧 `/` の挙動を退避
    pub api_search_mode: bool,
    /// search_mode / api_search_mode で共有する入力バッファ
    pub search_input: String,
    /// 確定済み検索パターン (n/N 用)
    pub search_pattern: Option<String>,
    /// 検索にヒットした plugins のインデックス一覧
    pub search_matches: Vec<usize>,
    /// search_matches 内の現在位置
    pub search_cursor: usize,
    /// インストール済みプラグインの full_name (小文字) 集合。`Enter` 時の
    /// 重複 add 警告と、リスト行の ✓ マーク表示に使う。
    pub installed: HashSet<String>,
    pub readme_content: Option<String>,
    pub readme_loading: bool,
    pub readme_scroll: u16,
    /// draw() ごとに strip+format し直さないための前処理済み markdown キャッシュ。
    /// `readme_prepared_key` に紐付き、キーが変わったときだけ作り直す。
    pub readme_prepared: String,
    /// `readme_prepared` の cache key:
    /// (selected full_name, readme_content 長, loading, visible_width)
    /// 内容そのものを保持せず長さだけで比較。幅が変わると wrap 後行数が変わるので
    /// key に含め、resize 時にも再計算する。
    readme_prepared_key: Option<(String, usize, bool, u16)>,
    /// README の post-wrap 推定行数。pane 内側幅 (`readme_visible_width`) で
    /// 文字幅換算して割った近似値。G / scroll 下限の clamp に使う。
    pub readme_line_count: u16,
    /// README pane の表示行数 (`draw()` で毎フレーム更新)。clamp 計算に使う。
    pub readme_visible_height: u16,
    /// README pane の表示幅 (`draw()` で毎フレーム更新)。post-wrap 計算に使う。
    pub readme_visible_width: u16,
    pub sort_mode: SortMode,
    pub message: Option<String>,
    pub focus: Focus,
    pub show_help: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Stars,
    Updated,
    Name,
}

impl SortMode {
    pub fn label(&self) -> &str {
        match self {
            SortMode::Stars => "stars",
            SortMode::Updated => "updated",
            SortMode::Name => "name",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            SortMode::Stars => SortMode::Updated,
            SortMode::Updated => SortMode::Name,
            SortMode::Name => SortMode::Stars,
        }
    }
}

/// GitHub README によくある `<img src="...badge">`, `<a>`, `<p align="center">`,
/// `<br>`, `<div>` 等の HTML タグを除去して markdown として読みやすくする。
///
/// 単一パスで UTF-8 安全 (ASCII の `<` / `>` 境界でしか切らない)。
/// - HTML コメント `<!-- ... -->` を削除
/// - `<img ...>` は `alt` 属性の値に置換、無ければ除去
/// - `REMOVE_TAGS` に含まれる既知の装飾タグは開き/閉じ両方除去
/// - それ以外の未知タグは markdown として意味を持ちうるので保持
fn strip_common_html(input: &str) -> String {
    const REMOVE_TAGS: &[&str] = &[
        "a", "br", "div", "p", "picture", "source", "sub", "sup", "kbd", "details", "summary",
        "center", "span", "table", "tr", "td", "th", "tbody", "thead", "ul", "li", "ol",
    ];

    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        // 次の '<' までをそのままコピー ('<' は ASCII なので byte 境界 = char 境界)
        let Some(lt_rel) = input[i..].find('<') else {
            out.push_str(&input[i..]);
            break;
        };
        let lt = i + lt_rel;
        out.push_str(&input[i..lt]);

        // HTML コメント
        if input[lt..].starts_with("<!--") {
            if let Some(end_rel) = input[lt + 4..].find("-->") {
                i = lt + 4 + end_rel + 3;
                continue;
            }
            // 閉じコメント無し → 残りそのまま
            out.push_str(&input[lt..]);
            break;
        }

        // 対応する '>' を探す (属性値に入った '>' を厳密に扱わない小さな手抜き)
        let Some(gt_rel) = input[lt..].find('>') else {
            out.push_str(&input[lt..]);
            break;
        };
        let gt = lt + gt_rel;
        let tag = &input[lt..=gt];

        // タグ名を抽出 (先頭 '<' / '</' を飛ばし、ASCII alphabetic の連続)
        let name = parse_tag_name(tag);
        let lname = name.to_ascii_lowercase();

        if lname == "img" {
            if let Some(alt) = extract_alt(tag)
                && !alt.is_empty()
            {
                out.push_str(&alt);
            }
        } else if REMOVE_TAGS.iter().any(|t| *t == lname) {
            // 開き/閉じ/self-closing どれでも丸ごと除去
        } else {
            // 未知タグは保持
            out.push_str(tag);
        }
        i = gt + 1;
    }
    out
}

/// `<tagname ...>` / `</tagname>` / `<tagname/>` からタグ名を抜き出す。
/// 見つからなければ空文字列。
fn parse_tag_name(tag: &str) -> String {
    let inner = tag
        .trim_start_matches('<')
        .trim_start_matches('/')
        .trim_start_matches('!');
    inner
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect()
}

/// Markdown の pipe テーブル (`| a | b |` + `| --- | --- |`) を検出して
/// 列ごとに max 幅に合わせて space padding を追加する。tui-markdown 0.3 は
/// テーブルを plain text として流すだけなので、行ごとの cell 長が違うと `|`
/// 位置がバラつく。pre-pass で揃えておくと少なくとも pane に収まる table は
/// 整って見える (pane 幅を超える場合は wrap するのでそこは仕方ない)。
fn realign_markdown_tables(input: &str) -> String {
    use unicode_width::UnicodeWidthStr;

    let lines: Vec<&str> = input.lines().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_table_header(line) && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            // 連続する table 行を収集
            let mut end = i + 2;
            while end < lines.len() && is_table_row(lines[end]) {
                end += 1;
            }
            let rows = &lines[i..end];
            // 各行をセル列に分解 (先頭/末尾の `|` は除く、trim)
            let parsed: Vec<Vec<&str>> = rows
                .iter()
                .map(|l| {
                    let t = l.trim();
                    let inner = t.strip_prefix('|').unwrap_or(t);
                    let inner = inner.strip_suffix('|').unwrap_or(inner);
                    inner.split('|').map(|c| c.trim()).collect()
                })
                .collect();
            let ncols = parsed.iter().map(|r| r.len()).max().unwrap_or(0);
            let mut widths = vec![0usize; ncols];
            // separator 行 (index 1) は width 計算から除外
            for (row_idx, row) in parsed.iter().enumerate() {
                if row_idx == 1 {
                    continue;
                }
                for (ci, cell) in row.iter().enumerate() {
                    widths[ci] = widths[ci].max(UnicodeWidthStr::width(*cell));
                }
            }
            for (row_idx, row) in parsed.iter().enumerate() {
                out.push('|');
                for (ci, width) in widths.iter().enumerate() {
                    out.push(' ');
                    let cell = row.get(ci).copied().unwrap_or("");
                    if row_idx == 1 {
                        out.push_str(&"-".repeat((*width).max(3)));
                    } else {
                        out.push_str(cell);
                        let pad = width.saturating_sub(UnicodeWidthStr::width(cell));
                        for _ in 0..pad {
                            out.push(' ');
                        }
                    }
                    out.push(' ');
                    out.push('|');
                }
                out.push('\n');
            }
            i = end;
            continue;
        }
        out.push_str(line);
        out.push('\n');
        i += 1;
    }
    out
}

fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.len() >= 2 && t.contains('|')
}

fn is_table_header(line: &str) -> bool {
    is_table_row(line)
}

fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with('|') || !t.ends_with('|') || t.len() < 3 {
        return false;
    }
    let inner = &t[1..t.len() - 1];
    inner.split('|').all(|cell| {
        let c = cell.trim();
        !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
    })
}

/// Paragraph が `Wrap { trim: false }` で描画したときの実行数を推定する。
/// - 幅が 0 (draw 前) なら Text の Line 数をそのまま返す
/// - 各 Line の spans 合計 display 幅を `pane_width` で割り切り上げ (空 Line は 1 行)
/// - 合計を u16 にクランプ
///
/// word-wrap の影響は無視しているので ±数行の誤差はあるが、`G` の clamp 用途には十分。
fn estimate_wrapped_rows(text: &ratatui::text::Text<'_>, pane_width: u16) -> u16 {
    use unicode_width::UnicodeWidthStr;
    if pane_width == 0 {
        return text.lines.len().try_into().unwrap_or(u16::MAX);
    }
    let w = pane_width as usize;
    let total: usize = text
        .lines
        .iter()
        .map(|line| {
            let display: usize = line
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            display.max(1).div_ceil(w)
        })
        .sum();
    total.try_into().unwrap_or(u16::MAX)
}

/// リスト行のセル表示用に問題のある Unicode スカラを落とす。
///
/// nerd font の Private Use Area (U+E000-F8FF 等) は `unicode-width` が幅 1 と
/// 答える一方で、nerd font を積んだ端末は 2 セルで描画するため、テーブル内に
/// 1 つでも混じると後続の列が累積的にずれてしまう (terminal と ratatui の
/// 合意が崩れる)。見た目より整列を優先して、該当レンジを抜く。
fn sanitize_cell_text(s: &str) -> String {
    s.chars()
        .filter(|c| {
            let code = *c as u32;
            // BMP PUA
            !(0xE000..=0xF8FF).contains(&code)
                // Supplementary PUA-A / PUA-B
                && !(0xF0000..=0xFFFFD).contains(&code)
                && !(0x100000..=0x10FFFD).contains(&code)
                // Variation selectors (FE00-FE0F, E0100-E01EF) — nerd font 絵文字の
                // 後ろにくっついて幅計算を乱すことがある。
                && !(0xFE00..=0xFE0F).contains(&code)
                && !(0xE0100..=0xE01EF).contains(&code)
        })
        .collect()
}

/// `<img ... alt="..." ...>` から `alt` 属性の値を取り出す。
/// クォート必須、エスケープ非対応。UTF-8 安全 (`=` / クォートは ASCII)。
fn extract_alt(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let pos = lower.find("alt=")?;
    let rest = &tag[pos + 4..];
    let delim = rest.chars().next()?;
    if delim != '"' && delim != '\'' {
        return None;
    }
    let after = &rest[delim.len_utf8()..];
    let end = after.find(delim)?;
    Some(after[..end].to_string())
}

impl StoreTuiState {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            table_state: TableState::default(),
            search_mode: false,
            api_search_mode: false,
            search_input: String::new(),
            search_pattern: None,
            search_matches: Vec::new(),
            search_cursor: 0,
            installed: HashSet::new(),
            readme_content: None,
            readme_loading: false,
            readme_scroll: 0,
            readme_prepared: String::new(),
            readme_prepared_key: None,
            readme_line_count: 0,
            readme_visible_height: 0,
            readme_visible_width: 0,
            sort_mode: SortMode::Stars,
            message: None,
            focus: Focus::List,
            show_help: false,
        }
    }

    pub fn set_plugins(&mut self, plugins: Vec<GitHubRepo>) {
        self.plugins = plugins;
        self.sort_plugins();
        if !self.plugins.is_empty() {
            self.table_state.select(Some(0));
        }
        self.readme_content = None;
        self.readme_scroll = 0;
        // プラグイン差し替え時は検索結果を無効化 (インデックスが無意味になるため)
        self.search_pattern = None;
        self.search_matches.clear();
        self.search_cursor = 0;
    }

    pub fn sort_plugins(&mut self) {
        match self.sort_mode {
            SortMode::Stars => self
                .plugins
                .sort_by_key(|p| std::cmp::Reverse(p.stargazers_count)),
            SortMode::Updated => self.plugins.sort_by(|a, b| b.updated_at.cmp(&a.updated_at)),
            SortMode::Name => self.plugins.sort_by(|a, b| {
                a.plugin_name()
                    .cmp(b.plugin_name())
                    .then_with(|| a.full_name.cmp(&b.full_name))
            }),
        }
    }

    pub fn selected_repo(&self) -> Option<&GitHubRepo> {
        self.table_state
            .selected()
            .and_then(|i| self.plugins.get(i))
    }

    pub fn next(&mut self) {
        if self.plugins.is_empty() {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map(|i| {
                if i >= self.plugins.len() - 1 {
                    0
                } else {
                    i + 1
                }
            })
            .unwrap_or(0);
        self.table_state.select(Some(i));
        self.readme_content = None;
        self.readme_scroll = 0;
    }

    pub fn previous(&mut self) {
        if self.plugins.is_empty() {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map(|i| {
                if i == 0 {
                    self.plugins.len() - 1
                } else {
                    i - 1
                }
            })
            .unwrap_or(0);
        self.table_state.select(Some(i));
        self.readme_content = None;
        self.readme_scroll = 0;
    }

    pub fn go_top(&mut self) {
        if !self.plugins.is_empty() {
            self.table_state.select(Some(0));
            self.readme_content = None;
            self.readme_scroll = 0;
        }
    }

    pub fn go_bottom(&mut self) {
        if !self.plugins.is_empty() {
            self.table_state.select(Some(self.plugins.len() - 1));
            self.readme_content = None;
            self.readme_scroll = 0;
        }
    }

    pub fn move_down(&mut self, n: usize) {
        if self.plugins.is_empty() {
            return;
        }
        let current = self.table_state.selected().unwrap_or(0);
        let target = (current + n).min(self.plugins.len() - 1);
        if target != current {
            self.table_state.select(Some(target));
            self.readme_content = None;
            self.readme_scroll = 0;
        }
    }

    pub fn move_up(&mut self, n: usize) {
        let current = self.table_state.selected().unwrap_or(0);
        let target = current.saturating_sub(n);
        if target != current {
            self.table_state.select(Some(target));
            self.readme_content = None;
            self.readme_scroll = 0;
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::List => Focus::Readme,
            Focus::Readme => Focus::List,
        };
    }

    pub fn scroll_readme_down(&mut self, n: u16) {
        let max = self.readme_max_scroll();
        self.readme_scroll = self.readme_scroll.saturating_add(n).min(max);
    }

    pub fn scroll_readme_up(&mut self, n: u16) {
        self.readme_scroll = self.readme_scroll.saturating_sub(n);
    }

    /// README を最下部までスクロール (`G` / `End` 用)。pre-wrap の行数を基準に
    /// 最終行あたりが pane 下端に来る位置を設定する。wrap で行数が増えた場合は
    /// 若干上方に見えるが、空白のみ見える `u16::MAX` 飛びより実用的。
    pub fn scroll_readme_to_bottom(&mut self) {
        self.readme_scroll = self.readme_max_scroll();
    }

    /// 現在の readme 行数と pane 高さから、これ以上下に行くと空白しか見えない
    /// 限界スクロール位置を返す。行数 ≤ 表示高さなら 0。
    fn readme_max_scroll(&self) -> u16 {
        self.readme_line_count
            .saturating_sub(self.readme_visible_height)
    }

    // ───────── ローカル検索 (`/` + n/N) ─────────

    /// `/` モードを開始 (local incremental search)。
    pub fn start_search(&mut self) {
        self.search_mode = true;
        self.api_search_mode = false;
        self.search_input.clear();
        self.message = None;
    }

    /// `S` モードを開始 (GitHub API search)。
    pub fn start_api_search(&mut self) {
        self.api_search_mode = true;
        self.search_mode = false;
        self.search_input.clear();
        self.message = None;
    }

    /// 検索モード (local/API 共通) を Esc でキャンセルし、local 側のハイライトも消す。
    pub fn search_cancel(&mut self) {
        self.search_mode = false;
        self.api_search_mode = false;
        self.search_input.clear();
        self.search_pattern = None;
        self.search_matches.clear();
        self.search_cursor = 0;
    }

    /// local 検索モードで Enter を押したときの確定処理。
    /// search_pattern は保持し続けるので、引き続き n/N で移動できる。
    pub fn search_confirm(&mut self) {
        self.search_mode = false;
    }

    /// local 検索モードで文字を入力 (インクリメンタル)。
    pub fn search_type(&mut self, c: char) {
        self.search_input.push(c);
        self.run_local_search(&self.search_input.clone());
    }

    /// local 検索モードで Backspace。空になったらハイライトクリア。
    pub fn search_backspace(&mut self) {
        self.search_input.pop();
        if self.search_input.is_empty() {
            self.search_pattern = None;
            self.search_matches.clear();
            self.search_cursor = 0;
        } else {
            self.run_local_search(&self.search_input.clone());
        }
    }

    /// `plugin_name + description + topics` を対象に大文字小文字無視で部分一致検索。
    /// 最初のマッチにカーソルを移動。
    fn run_local_search(&mut self, pattern: &str) {
        let pat = pattern.to_lowercase();
        self.search_matches = self
            .plugins
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                let name_hit = r.plugin_name().to_lowercase().contains(&pat);
                let desc_hit = r
                    .description
                    .as_deref()
                    .map(|d| d.to_lowercase().contains(&pat))
                    .unwrap_or(false);
                let topic_hit = r.topics.iter().any(|t| t.to_lowercase().contains(&pat));
                name_hit || desc_hit || topic_hit
            })
            .map(|(i, _)| i)
            .collect();
        self.search_pattern = Some(pattern.to_string());
        self.search_cursor = 0;
        if let Some(&idx) = self.search_matches.first() {
            self.table_state.select(Some(idx));
            self.readme_content = None;
            self.readme_scroll = 0;
        }
    }

    /// n — 次のマッチへ (ラップ)。
    pub fn search_next(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_cursor = (self.search_cursor + 1) % self.search_matches.len();
        let idx = self.search_matches[self.search_cursor];
        self.table_state.select(Some(idx));
        self.readme_content = None;
        self.readme_scroll = 0;
    }

    /// N — 前のマッチへ (ラップ)。
    pub fn search_prev(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_cursor = if self.search_cursor == 0 {
            self.search_matches.len() - 1
        } else {
            self.search_cursor - 1
        };
        let idx = self.search_matches[self.search_cursor];
        self.table_state.select(Some(idx));
        self.readme_content = None;
        self.readme_scroll = 0;
    }

    // ───────── installed マーク ─────────

    /// 現在選択中の plugin がインストール済みかを判定。
    /// GitHub の `full_name` は大文字小文字非依存なので lowercase で比較。
    pub fn is_installed(&self, repo: &GitHubRepo) -> bool {
        self.installed.contains(&repo.full_name.to_lowercase())
    }

    /// add 後に呼び出し、以降のリスト描画で ✓ マークが付くようにする。
    pub fn mark_installed(&mut self, repo: &GitHubRepo) {
        self.installed.insert(repo.full_name.to_lowercase());
    }

    /// README 描画用の前処理済み markdown を必要なら再計算する。
    /// `selected_repo` / `readme_content` / `readme_loading` / pane 幅 のいずれかが
    /// 変わったときだけ HTML strip + topics prefix の組み立てと post-wrap 行数の
    /// 見積もりを行い、結果を `readme_prepared` / `readme_line_count` に保持する。
    fn ensure_readme_prepared(&mut self) {
        let selected_name = self
            .selected_repo()
            .map(|r| r.full_name.clone())
            .unwrap_or_default();
        let content_len = self.readme_content.as_ref().map(|c| c.len()).unwrap_or(0);
        let key = (
            selected_name,
            content_len,
            self.readme_loading,
            self.readme_visible_width,
        );
        if self.readme_prepared_key.as_ref() == Some(&key) {
            return;
        }

        let body = if self.readme_loading {
            "_Loading README..._".to_string()
        } else {
            self.readme_content.clone().unwrap_or_else(|| {
                if self.plugins.is_empty() {
                    "_Press / to search or S to fetch more._".to_string()
                } else {
                    "_Loading..._".to_string()
                }
            })
        };

        let topics_prefix = self
            .selected_repo()
            .map(|r| {
                if r.topics.is_empty() {
                    String::new()
                } else {
                    let joined = r
                        .topics
                        .iter()
                        .map(|t| format!("`{}`", t))
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("**Topics:** {}\n\n---\n\n", joined)
                }
            })
            .unwrap_or_default();

        let cleaned = strip_common_html(&body);
        // README 本文にも nerd font の Private Use Area 文字が混じることがあり、
        // `unicode-width` は幅 1 と答えるが terminal は 2 セルで描画するため
        // tui-markdown の Line 折返しが壊れる。リスト側と同じく PUA / VS を除去する。
        let sanitized = sanitize_cell_text(&cleaned);
        // pipe テーブルは tui-markdown が plain text として流してしまい行毎に
        // `|` 位置がバラつくので、事前に列幅を揃えておく。
        let aligned = realign_markdown_tables(&sanitized);
        self.readme_prepared = format!("{}{}", topics_prefix, aligned);
        // ratatui の Paragraph({ wrap: trim=false }) が pane 幅で折り返した後の
        // 実行数を推定する。各 Line の表示幅を unicode-width で測り、
        // pane の内側幅で割って切り上げて合計する近似 (空 Line は 1 行分)。
        // word-wrap と完全一致はしないが、G のオーバー/アンダーを実用レベルまで
        // 抑えられる。`\n` カウントは paragraph 内の soft break まで数えすぎ、
        // tui-markdown Line 数は wrap を無視するため、いずれも単独だとズレる。
        let rendered = tui_markdown::from_str(&self.readme_prepared);
        self.readme_line_count = estimate_wrapped_rows(&rendered, self.readme_visible_width);
        self.readme_prepared_key = Some(key);
    }

    pub fn draw(&mut self, f: &mut Frame) {
        // 毎フレームまず全セルを空白 + 既定スタイルに戻してから widget を重ねる。
        // 個別 pane 単位の Clear だと highlight-code (ansi-to-tui) の styled span が
        // scroll 位置変更時に残骸を残すケースがあるため、フレーム丸ごと洗う。
        // ratatui の diff 機構により実際の端末出力は変化したセルのみ。
        f.render_widget(ratatui::widgets::Clear, f.area());

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // title + search
                Constraint::Min(10),   // main content
                Constraint::Length(3), // footer
            ])
            .split(f.area());

        // ── Title / Search bar ──
        let title_content = if self.search_mode || self.api_search_mode {
            let prompt = if self.api_search_mode { " S " } else { " / " };
            let match_info = if self.api_search_mode {
                format!(" (GitHub API, {} cached)", self.plugins.len())
            } else if self.search_input.is_empty() {
                String::new()
            } else {
                format!(
                    " ({}/{} matches)",
                    self.search_matches.len(),
                    self.plugins.len()
                )
            };
            Line::from(vec![
                Span::styled(
                    " rvpm store ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    prompt,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(&self.search_input, Style::default().fg(Color::White)),
                Span::styled("\u{2588}", Style::default().fg(Color::Yellow)), // cursor
                Span::styled(match_info, Style::default().fg(Color::DarkGray)),
            ])
        } else {
            let info = if let Some(msg) = &self.message {
                Span::styled(format!("  {}", msg), Style::default().fg(Color::Green))
            } else if let Some(pat) = &self.search_pattern {
                Span::styled(
                    format!(
                        "  /{}  {} matches  sort:{}",
                        pat,
                        self.search_matches.len(),
                        self.sort_mode.label()
                    ),
                    Style::default().fg(Color::DarkGray),
                )
            } else {
                Span::styled(
                    format!(
                        "  {} plugins  sort:{}",
                        self.plugins.len(),
                        self.sort_mode.label()
                    ),
                    Style::default().fg(Color::DarkGray),
                )
            };
            Line::from(vec![
                Span::styled(
                    " rvpm store ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                info,
            ])
        };
        let title = Paragraph::new(title_content).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(title, chunks[0]);

        // ── Main: list + readme ──
        // 横幅が狭い terminal では side-by-side だと plugin 名が潰れるので、
        // 一定幅未満のときは縦積み (list: 上 / readme: 下) に切り替える。
        let total_width = f.area().width;
        let side_by_side = total_width >= 160;
        let main_chunks = if side_by_side {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(chunks[1])
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(chunks[1])
        };

        // Left: plugin list
        let rows: Vec<Row> = self
            .plugins
            .iter()
            .map(|repo| {
                let desc = repo.description.as_deref().unwrap_or("");
                // PUA / VS 除去してから char 数で truncate。これで terminal 描画幅と
                // ratatui の見積もり幅が一致し、以降の列が累積ずれしない。
                let desc_truncated: String = sanitize_cell_text(desc).chars().take(40).collect();
                let installed_cell = if self.is_installed(repo) {
                    ratatui::widgets::Cell::from(Span::styled(
                        "\u{2713}",
                        Style::default().fg(Color::Green),
                    ))
                } else {
                    ratatui::widgets::Cell::from(" ")
                };
                let topics_str: String = sanitize_cell_text(
                    &repo
                        .topics
                        .iter()
                        .take(3)
                        .map(|t| format!("#{}", t))
                        .collect::<Vec<_>>()
                        .join(" "),
                );
                let name_str = sanitize_cell_text(repo.plugin_name());
                Row::new(vec![
                    installed_cell,
                    ratatui::widgets::Cell::from(format!(" \u{2605}{}", repo.stars_display()))
                        .style(Style::default().fg(Color::Yellow)),
                    ratatui::widgets::Cell::from(name_str).style(Style::default().fg(Color::White)),
                    ratatui::widgets::Cell::from(desc_truncated)
                        .style(Style::default().fg(Color::DarkGray)),
                    ratatui::widgets::Cell::from(topics_str)
                        .style(Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect();

        // plugin name を最優先にするため、name と desc は Min 制約で伸縮、
        // topics は終端で Length、stars/installed は固定。side_by_side のときは
        // 横幅が限られるため topics を短めに (18)、vertical 積みのときは余裕あるので広めに (30)。
        let name_col = Constraint::Min(15);
        let desc_col = Constraint::Min(20);
        let topics_col = if side_by_side {
            Constraint::Length(18)
        } else {
            Constraint::Length(30)
        };
        let table = Table::new(
            rows,
            [
                Constraint::Length(2),
                Constraint::Length(8),
                name_col,
                desc_col,
                topics_col,
            ],
        )
        .block(
            Block::default()
                .title(" Plugins ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if self.focus == Focus::List {
                    Color::Yellow
                } else {
                    Color::DarkGray
                })),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Indexed(237))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("\u{25b8} ");
        f.render_stateful_widget(table, main_chunks[0], &mut self.table_state);

        // Right: README preview (tui-markdown rendered GFM)
        // scroll 系のメソッドが使う pane 内側サイズ。borders 分を差し引く。
        // ensure_readme_prepared は visible_width を cache key に使うのでその前に更新する。
        self.readme_visible_height = main_chunks[1].height.saturating_sub(2);
        self.readme_visible_width = main_chunks[1].width.saturating_sub(2);
        // HTML strip + topics 結合 + post-wrap 行数はキャッシュされるので、
        // draw() ごとのコストは tui_markdown::from_str のパースだけ。
        self.ensure_readme_prepared();
        let mut rendered = tui_markdown::from_str(&self.readme_prepared);
        // highlight-code 由来の背景色付き Span が scroll 時に一部ホストで
        // 残骸を残すので、前景色だけ残して背景は既定に戻す。fg による
        // syntax highlighting は維持されるので可読性は保たれる。
        for line in &mut rendered.lines {
            for span in &mut line.spans {
                span.style.bg = None;
            }
            line.style.bg = None;
        }

        let readme_title = self
            .selected_repo()
            .map(|r| format!(" {} ", r.full_name))
            .unwrap_or_else(|| " README ".to_string());

        let readme = Paragraph::new(rendered)
            .block(
                Block::default()
                    .title(readme_title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if self.focus == Focus::Readme {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    })),
            )
            .wrap(Wrap { trim: false })
            .scroll((self.readme_scroll, 0));
        // Paragraph は inner area の未使用セルを空白で埋めないため、前フレームの
        // 長い README の残骸が残ることがある (特に zellij のようにターミナルが
        // セル状態を厳密に保持するホストで顕在化する)。Clear でペイン全体を空白に
        // してから Paragraph を重ねる。
        f.render_widget(ratatui::widgets::Clear, main_chunks[1]);
        f.render_widget(readme, main_chunks[1]);

        // ── Footer ──
        let footer = if self.search_mode || self.api_search_mode {
            let confirm_label = if self.api_search_mode {
                ":api-search "
            } else {
                ":confirm "
            };
            Paragraph::new(Line::from(vec![
                Span::styled(" Enter", Style::default().fg(Color::Yellow)),
                Span::styled(confirm_label, Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::Yellow)),
                Span::styled(":cancel", Style::default().fg(Color::DarkGray)),
            ]))
        } else {
            let focus_label = match self.focus {
                Focus::List => "readme",
                Focus::Readme => "list",
            };
            Paragraph::new(Line::from(vec![
                Span::styled(" /", Style::default().fg(Color::Yellow)),
                Span::styled(":search ", Style::default().fg(Color::DarkGray)),
                Span::styled("n/N", Style::default().fg(Color::Yellow)),
                Span::styled(":next/prev ", Style::default().fg(Color::DarkGray)),
                Span::styled("S", Style::default().fg(Color::Yellow)),
                Span::styled(":api-search ", Style::default().fg(Color::DarkGray)),
                Span::styled("Tab", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!(":{} ", focus_label),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::styled(":add ", Style::default().fg(Color::DarkGray)),
                Span::styled("?", Style::default().fg(Color::Yellow)),
                Span::styled(":help ", Style::default().fg(Color::DarkGray)),
                Span::styled("q", Style::default().fg(Color::Yellow)),
                Span::styled(":quit", Style::default().fg(Color::DarkGray)),
            ]))
        };
        f.render_widget(
            footer.block(Block::default().borders(Borders::ALL)),
            chunks[2],
        );

        // ── Help popup overlay ──
        if self.show_help {
            use ratatui::layout::Rect;
            use ratatui::widgets::Clear;
            let area = f.area();
            let popup_w = 60u16.min(area.width.saturating_sub(4));
            let popup_h = 26u16.min(area.height.saturating_sub(4));
            let popup = Rect::new(
                (area.width.saturating_sub(popup_w)) / 2,
                (area.height.saturating_sub(popup_h)) / 2,
                popup_w,
                popup_h,
            );
            let help_lines = vec![
                Line::from(vec![Span::styled(
                    "  Navigation",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  j / k       ", Style::default().fg(Color::Yellow)),
                    Span::styled("Move / scroll down / up", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  g / G       ", Style::default().fg(Color::Yellow)),
                    Span::styled("Go to top / bottom", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  C-d / C-u   ", Style::default().fg(Color::Yellow)),
                    Span::styled("Half page down / up", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  C-f / C-b   ", Style::default().fg(Color::Yellow)),
                    Span::styled("Full page down / up", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  Tab         ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        "Switch focus: list / readme",
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  Search",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  /           ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        "Local incremental (name + desc + topics)",
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  n / N       ", Style::default().fg(Color::Yellow)),
                    Span::styled("Next / prev match", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  S           ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        "GitHub API search (fetch)",
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  Actions",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Enter       ", Style::default().fg(Color::Yellow)),
                    Span::styled("Add plugin to config", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  o           ", Style::default().fg(Color::Yellow)),
                    Span::styled("Open in browser", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  s           ", Style::default().fg(Color::Yellow)),
                    Span::styled("Cycle sort mode", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  R           ", Style::default().fg(Color::Yellow)),
                    Span::styled("Refresh (clear cache)", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  q / Esc     ", Style::default().fg(Color::Yellow)),
                    Span::styled("Quit", Style::default().fg(Color::White)),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  Legend",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  \u{2713}           ", Style::default().fg(Color::Green)),
                    Span::styled(
                        "Already installed in your config",
                        Style::default().fg(Color::White),
                    ),
                ]),
            ];
            f.render_widget(Clear, popup);
            f.render_widget(
                Paragraph::new(help_lines).block(
                    Block::default()
                        .title(" Help [?] ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow)),
                ),
                popup,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_repo(name: &str, stars: u64) -> GitHubRepo {
        GitHubRepo {
            full_name: format!("owner/{}", name),
            html_url: format!("https://github.com/owner/{}", name),
            description: Some(format!("{} plugin", name)),
            stargazers_count: stars,
            updated_at: "2026-01-01".to_string(),
            topics: vec![],
            default_branch: Some("main".to_string()),
        }
    }

    #[test]
    fn test_sort_by_stars() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![
            make_repo("low", 10),
            make_repo("high", 1000),
            make_repo("mid", 100),
        ]);
        assert_eq!(state.plugins[0].plugin_name(), "high");
        assert_eq!(state.plugins[1].plugin_name(), "mid");
        assert_eq!(state.plugins[2].plugin_name(), "low");
    }

    #[test]
    fn test_sort_by_name() {
        let mut state = StoreTuiState::new();
        state.sort_mode = SortMode::Name;
        state.set_plugins(vec![make_repo("zebra", 10), make_repo("alpha", 1000)]);
        assert_eq!(state.plugins[0].plugin_name(), "alpha");
        assert_eq!(state.plugins[1].plugin_name(), "zebra");
    }

    #[test]
    fn test_navigation() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![
            make_repo("a", 100),
            make_repo("b", 50),
            make_repo("c", 10),
        ]);
        assert_eq!(state.table_state.selected(), Some(0));
        state.next();
        assert_eq!(state.table_state.selected(), Some(1));
        state.next();
        assert_eq!(state.table_state.selected(), Some(2));
        state.next(); // wrap
        assert_eq!(state.table_state.selected(), Some(0));
        state.previous(); // wrap back
        assert_eq!(state.table_state.selected(), Some(2));
    }

    #[test]
    fn test_readme_scroll() {
        let mut state = StoreTuiState::new();
        // clamp を効かせるために表示範囲を設定 (100 行 README を高さ 20 の pane で)
        state.readme_line_count = 100;
        state.readme_visible_height = 20;
        state.scroll_readme_down(10);
        assert_eq!(state.readme_scroll, 10);
        state.scroll_readme_up(3);
        assert_eq!(state.readme_scroll, 7);
        state.scroll_readme_up(100);
        assert_eq!(state.readme_scroll, 0);
    }

    #[test]
    fn test_scroll_readme_down_clamps_to_max() {
        let mut state = StoreTuiState::new();
        state.readme_line_count = 50;
        state.readme_visible_height = 20;
        // max = 50 - 20 = 30
        state.scroll_readme_down(u16::MAX);
        assert_eq!(state.readme_scroll, 30);
    }

    #[test]
    fn test_scroll_readme_to_bottom_lands_at_max() {
        let mut state = StoreTuiState::new();
        state.readme_line_count = 80;
        state.readme_visible_height = 25;
        state.scroll_readme_to_bottom();
        // max = 80 - 25 = 55
        assert_eq!(state.readme_scroll, 55);
    }

    #[test]
    fn test_scroll_readme_to_bottom_on_short_content_stays_at_top() {
        let mut state = StoreTuiState::new();
        // 内容が pane より短いならスクロール不要
        state.readme_line_count = 10;
        state.readme_visible_height = 25;
        state.scroll_readme_to_bottom();
        assert_eq!(state.readme_scroll, 0);
    }

    #[test]
    fn test_toggle_focus() {
        let mut state = StoreTuiState::new();
        assert_eq!(state.focus, Focus::List);
        state.toggle_focus();
        assert_eq!(state.focus, Focus::Readme);
        state.toggle_focus();
        assert_eq!(state.focus, Focus::List);
    }

    #[test]
    fn test_go_top_and_bottom() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![
            make_repo("a", 100),
            make_repo("b", 50),
            make_repo("c", 10),
        ]);
        state.next();
        state.next();
        assert_eq!(state.table_state.selected(), Some(2));
        // seed readme state to verify it resets
        state.readme_content = Some("old".to_string());
        state.readme_scroll = 42;
        state.go_top();
        assert_eq!(state.table_state.selected(), Some(0));
        assert!(state.readme_content.is_none());
        assert_eq!(state.readme_scroll, 0);
        state.go_bottom();
        assert_eq!(state.table_state.selected(), Some(2));
        assert!(state.readme_content.is_none());
        assert_eq!(state.readme_scroll, 0);
    }

    #[test]
    fn test_move_down_up() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![
            make_repo("a", 100),
            make_repo("b", 90),
            make_repo("c", 80),
            make_repo("d", 70),
            make_repo("e", 60),
        ]);
        state.readme_content = Some("test".to_string());
        state.readme_scroll = 10;
        state.move_down(3);
        assert_eq!(state.table_state.selected(), Some(3));
        assert!(state.readme_content.is_none());
        assert_eq!(state.readme_scroll, 0);
        state.move_up(2);
        assert_eq!(state.table_state.selected(), Some(1));
        state.move_down(100);
        assert_eq!(state.table_state.selected(), Some(4));
        state.move_up(100);
        assert_eq!(state.table_state.selected(), Some(0));
    }

    fn make_repo_full(
        name: &str,
        stars: u64,
        description: Option<&str>,
        topics: Vec<&str>,
    ) -> GitHubRepo {
        GitHubRepo {
            full_name: format!("owner/{}", name),
            html_url: format!("https://github.com/owner/{}", name),
            description: description.map(|d| d.to_string()),
            stargazers_count: stars,
            updated_at: "2026-01-01".to_string(),
            topics: topics.iter().map(|t| t.to_string()).collect(),
            default_branch: Some("main".to_string()),
        }
    }

    // ───── local search (/ + n/N) ─────

    #[test]
    fn test_search_matches_plugin_name() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![
            make_repo_full("telescope", 100, Some("fuzzy"), vec![]),
            make_repo_full("snacks", 90, Some("misc"), vec![]),
        ]);
        state.start_search();
        state.search_type('t');
        state.search_type('e');
        state.search_type('l');
        assert_eq!(state.search_matches, vec![0]);
        assert_eq!(state.table_state.selected(), Some(0));
    }

    #[test]
    fn test_search_matches_description() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![
            make_repo_full("telescope", 100, Some("fuzzy finder"), vec![]),
            make_repo_full("snacks", 90, Some("misc utilities"), vec![]),
        ]);
        state.start_search();
        state.search_type('f');
        state.search_type('u');
        state.search_type('z');
        assert_eq!(state.search_matches, vec![0]);
    }

    #[test]
    fn test_search_matches_topic() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![
            make_repo_full("telescope", 100, Some("x"), vec!["lua"]),
            make_repo_full("snacks", 90, Some("y"), vec!["utility"]),
        ]);
        state.start_search();
        state.search_type('l');
        state.search_type('u');
        state.search_type('a');
        assert_eq!(state.search_matches, vec![0]);
    }

    #[test]
    fn test_search_case_insensitive() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![
            make_repo_full("Telescope", 100, Some("Fuzzy"), vec!["Lua"]),
            make_repo_full("snacks", 90, Some("z"), vec![]),
        ]);
        state.start_search();
        state.search_type('L');
        state.search_type('u');
        state.search_type('A');
        assert_eq!(state.search_matches, vec![0]);
    }

    #[test]
    fn test_search_next_wraps() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![
            make_repo_full("aaa-nvim", 300, None, vec![]),
            make_repo_full("bbb", 200, None, vec![]),
            make_repo_full("ccc-nvim", 100, None, vec![]),
        ]);
        state.start_search();
        state.search_type('n');
        state.search_type('v');
        state.search_type('i');
        state.search_type('m');
        // matches are aaa-nvim (idx 0) and ccc-nvim (idx 2)
        assert_eq!(state.search_matches, vec![0, 2]);
        assert_eq!(state.table_state.selected(), Some(0));
        state.search_next();
        assert_eq!(state.table_state.selected(), Some(2));
        state.search_next(); // wrap
        assert_eq!(state.table_state.selected(), Some(0));
    }

    #[test]
    fn test_search_prev_wraps() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![
            make_repo_full("aaa-nvim", 300, None, vec![]),
            make_repo_full("bbb", 200, None, vec![]),
            make_repo_full("ccc-nvim", 100, None, vec![]),
        ]);
        state.start_search();
        state.search_type('n');
        state.search_type('v');
        state.search_type('i');
        state.search_type('m');
        assert_eq!(state.table_state.selected(), Some(0));
        state.search_prev(); // wrap to last
        assert_eq!(state.table_state.selected(), Some(2));
        state.search_prev();
        assert_eq!(state.table_state.selected(), Some(0));
    }

    #[test]
    fn test_search_backspace_clears_matches_when_empty() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![make_repo_full("telescope", 100, None, vec![])]);
        state.start_search();
        state.search_type('t');
        assert!(!state.search_matches.is_empty());
        state.search_backspace();
        assert!(state.search_matches.is_empty());
        assert!(state.search_pattern.is_none());
    }

    #[test]
    fn test_search_cancel_clears_state() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![make_repo_full("telescope", 100, None, vec![])]);
        state.start_search();
        state.search_type('t');
        state.search_cancel();
        assert!(!state.search_mode);
        assert!(!state.api_search_mode);
        assert!(state.search_input.is_empty());
        assert!(state.search_pattern.is_none());
        assert!(state.search_matches.is_empty());
    }

    #[test]
    fn test_search_confirm_keeps_pattern_for_next() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![
            make_repo_full("aaa-nvim", 300, None, vec![]),
            make_repo_full("bbb-nvim", 200, None, vec![]),
        ]);
        state.start_search();
        state.search_type('n');
        state.search_type('v');
        state.search_confirm();
        assert!(!state.search_mode);
        assert_eq!(state.search_pattern.as_deref(), Some("nv"));
        // n キーでジャンプできる
        state.search_next();
        assert_eq!(state.table_state.selected(), Some(1));
    }

    #[test]
    fn test_start_api_search_cancels_local_search() {
        let mut state = StoreTuiState::new();
        state.start_search();
        state.search_type('a');
        state.start_api_search();
        assert!(!state.search_mode);
        assert!(state.api_search_mode);
        assert!(state.search_input.is_empty());
    }

    #[test]
    fn test_set_plugins_clears_search_state() {
        let mut state = StoreTuiState::new();
        state.set_plugins(vec![make_repo_full("telescope", 100, None, vec![])]);
        state.start_search();
        state.search_type('t');
        assert!(!state.search_matches.is_empty());
        // 再取得 (API search で差し替え) シミュレーション
        state.set_plugins(vec![make_repo_full("other", 50, None, vec![])]);
        assert!(state.search_matches.is_empty());
        assert!(state.search_pattern.is_none());
        assert_eq!(state.search_cursor, 0);
    }

    // ───── installed mark ─────

    #[test]
    fn test_is_installed_case_insensitive() {
        let mut state = StoreTuiState::new();
        state.installed.insert("folke/snacks.nvim".to_string());
        let repo = make_repo_full("Snacks.nvim", 100, None, vec![]);
        // full_name is "owner/Snacks.nvim" — different owner, miss
        assert!(!state.is_installed(&repo));

        // 完全一致 (大文字混じり) は小文字比較でヒット
        state.installed.clear();
        state.installed.insert("owner/snacks.nvim".to_string());
        let repo2 = make_repo_full("Snacks.NVIM", 100, None, vec![]);
        assert!(state.is_installed(&repo2));
    }

    #[test]
    fn test_mark_installed_adds_to_set() {
        let mut state = StoreTuiState::new();
        let repo = make_repo_full("telescope.nvim", 100, None, vec![]);
        assert!(!state.is_installed(&repo));
        state.mark_installed(&repo);
        assert!(state.is_installed(&repo));
    }

    // ───── strip_common_html UTF-8 safety ─────

    #[test]
    fn test_strip_html_preserves_japanese_text() {
        let input = "これは README です。\n<a href=\"...\">リンク</a> の後。";
        let out = strip_common_html(input);
        assert!(out.contains("これは README です。"));
        assert!(out.contains("リンク"));
        assert!(!out.contains("<a"));
        assert!(!out.contains("</a>"));
    }

    #[test]
    fn test_strip_html_preserves_emoji_around_img() {
        // <img> のすぐ前後に絵文字・日本語を置いてもバイト位置破綻しない
        let input = "🎉 hi <img alt=\"X\" src=\"y\"/> あ い";
        let out = strip_common_html(input);
        assert!(out.contains("🎉"));
        assert!(out.contains("X"));
        assert!(out.contains("あ い"));
        assert!(!out.contains("<img"));
    }

    #[test]
    fn test_strip_html_img_alt_extracted() {
        let out = strip_common_html("<img src=\"x.png\" alt=\"Build Status\">");
        assert_eq!(out, "Build Status");
    }

    #[test]
    fn test_strip_html_img_no_alt_dropped() {
        let out = strip_common_html("<img src=\"x.png\">");
        assert_eq!(out, "");
    }

    #[test]
    fn test_strip_html_abbr_not_false_matched_as_a() {
        // 以前の実装では <abbr> を <a> として扱って残り全削除していた
        let input = "<abbr title=\"x\">TLA</abbr> followed by <a>link</a>";
        let out = strip_common_html(input);
        assert!(out.contains("<abbr"));
        assert!(out.contains("TLA"));
        assert!(out.contains("link"));
        assert!(!out.contains("<a>"));
        assert!(!out.contains("</a>"));
    }

    #[test]
    fn test_strip_html_comment_removed() {
        let out = strip_common_html("before <!-- skip me --> after");
        assert!(out.contains("before"));
        assert!(out.contains("after"));
        assert!(!out.contains("skip me"));
    }

    #[test]
    fn test_strip_html_unknown_tag_preserved() {
        // markdown のインラインコード/カスタム要素は残す
        let out = strip_common_html("<mark>note</mark>");
        assert!(out.contains("<mark>"));
    }

    // ───── sanitize_cell_text ─────

    #[test]
    fn test_sanitize_strips_nerd_font_pua() {
        let input = "icon \u{e801} here";
        assert_eq!(sanitize_cell_text(input), "icon  here");
    }

    #[test]
    fn test_sanitize_strips_variation_selectors() {
        // U+FE0F (emoji presentation selector) — しばしば width 計算を乱す
        let input = "gear\u{FE0F} icon";
        assert_eq!(sanitize_cell_text(input), "gear icon");
    }

    #[test]
    fn test_sanitize_keeps_ascii() {
        let input = "A collection of small qol plugins for Neovim";
        assert_eq!(sanitize_cell_text(input), input);
    }

    // ───── realign_markdown_tables ─────

    #[test]
    fn test_realign_table_pads_short_cells() {
        let input = "\
| short | name |
| --- | --- |
| NoiceCmdlineIcon | desc |
";
        let out = realign_markdown_tables(input);
        // 列ごとに `|` 位置が揃っているか
        let lines: Vec<&str> = out.lines().collect();
        let pipe_positions: Vec<Vec<usize>> = lines
            .iter()
            .map(|l| {
                l.char_indices()
                    .filter(|(_, c)| *c == '|')
                    .map(|(i, _)| i)
                    .collect()
            })
            .collect();
        assert_eq!(pipe_positions[0], pipe_positions[2]);
    }

    #[test]
    fn test_realign_table_preserves_non_table_lines() {
        let input = "\
Hello world
| a | b |
| --- | --- |
| 1 | 2 |
Goodbye
";
        let out = realign_markdown_tables(input);
        assert!(out.contains("Hello world"));
        assert!(out.contains("Goodbye"));
    }

    #[test]
    fn test_realign_table_handles_empty_cells() {
        let input = "\
| a | b | c |
| --- | --- | --- |
| x | | z |
";
        let out = realign_markdown_tables(input);
        // 空セルが原因で column 数が狂わないこと
        assert_eq!(out.lines().count(), 3);
    }

    #[test]
    fn test_realign_without_separator_is_noop() {
        // separator が無い連続行はテーブル扱いしない
        let input = "| not | a table |\nbecause no separator";
        let out = realign_markdown_tables(input);
        assert!(out.contains("| not | a table |"));
        assert!(out.contains("because no separator"));
    }

    #[test]
    fn test_sanitize_keeps_japanese_and_standard_emoji() {
        // 日本語と通常絵文字 (unicode-width が正しく 2 と判定するもの) は残す
        let input = "プラグイン 🎉 ready";
        assert_eq!(sanitize_cell_text(input), input);
    }

    #[test]
    fn test_strip_html_multibyte_inside_tag() {
        // 属性値に日本語が入っていても切り損なわない
        let input = "<img alt=\"ロゴ\" src=\"logo.png\"/> 本文";
        let out = strip_common_html(input);
        assert!(out.contains("ロゴ"));
        assert!(out.contains("本文"));
    }
}
