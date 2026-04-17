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
/// 完全な HTML パーサではなく、行単位の **存在感が大きい** タグだけを対象にする最小限の処理:
/// - `<img .../>` や `<img ...>` を alt text で置き換え (alt があれば) or 空文字
/// - `<a ...>` / `</a>` / `<br ?/?>` / `<div ...>` / `</div>` / `<p ...>` / `</p>` / `<picture>` 類を削除
/// - `<!-- ... -->` コメントを削除
fn strip_common_html(input: &str) -> String {
    let mut s = input.to_string();

    // HTML コメントを削除
    while let Some(start) = s.find("<!--") {
        if let Some(rel_end) = s[start..].find("-->") {
            s.replace_range(start..start + rel_end + 3, "");
        } else {
            break;
        }
    }

    // <img ...> を alt text or 空文字に置換
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<'
            && i + 4 <= bytes.len()
            && &s[i..i + 4].to_lowercase() == "<img"
            && let Some(rel_end) = s[i..].find('>')
        {
            let tag = &s[i..=i + rel_end];
            let alt = extract_attr(tag, "alt").unwrap_or_default();
            if !alt.is_empty() {
                out.push_str(&alt);
            }
            i += rel_end + 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }

    // 既知の **開き/閉じ両方** のタグ名だけを、`<tag ...>` or `</tag>` or `<tag ... />` 形式で削除
    let tags = [
        "a", "br", "div", "p", "picture", "source", "sub", "sup", "kbd", "details", "summary",
        "center", "span", "table", "tr", "td", "th", "tbody", "thead", "ul", "li",
    ];
    for tag in tags {
        let open = format!("<{}", tag);
        let close = format!("</{}>", tag);
        out = remove_tag_instances(&out, &open, &close);
    }

    out
}

/// `<img alt="Hello world" src="...">` から `alt` の値を取り出す。
/// 非常に緩いパース (属性はクォート必須、エスケープ非対応)。見つからなければ None。
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let pat = format!("{}=", attr);
    let pos = lower.find(&pat)?;
    let rest = &tag[pos + pat.len()..];
    let (delim, start_off) = if let Some(stripped) = rest.strip_prefix('"') {
        ('"', stripped.as_ptr() as usize - rest.as_ptr() as usize)
    } else if let Some(stripped) = rest.strip_prefix('\'') {
        ('\'', stripped.as_ptr() as usize - rest.as_ptr() as usize)
    } else {
        return None;
    };
    let body = &rest[start_off..];
    let end = body.find(delim)?;
    Some(body[..end].to_string())
}

/// `<tag ...>` および `</tag>` をすべて削除。
/// `input.to_lowercase()` で case-insensitive 比較するが、削除は元文字列に対して行う。
fn remove_tag_instances(input: &str, open_prefix: &str, close: &str) -> String {
    let mut s = input.to_string();
    let lower_open = open_prefix.to_lowercase();
    let lower_close = close.to_lowercase();

    loop {
        let lower = s.to_lowercase();
        if let Some(start) = lower.find(&lower_open) {
            // 次が ' ' / '>' / '/' のいずれかでないと別タグ (e.g., <abbr>)
            let after = s[start + open_prefix.len()..].chars().next().unwrap_or('>');
            if !matches!(after, ' ' | '>' | '/' | '\t' | '\n') {
                // false positive, skip this instance by slicing past it
                let remainder = &s[start + open_prefix.len()..];
                // just break to avoid infinite loop; accept leftover
                let _ = remainder;
                break;
            }
            if let Some(rel_end) = s[start..].find('>') {
                s.replace_range(start..start + rel_end + 1, "");
                continue;
            }
        }
        break;
    }

    while let Some(start) = s.to_lowercase().find(&lower_close) {
        s.replace_range(start..start + close.len(), "");
    }
    s
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
        self.readme_scroll = self.readme_scroll.saturating_add(n);
    }

    pub fn scroll_readme_up(&mut self, n: u16) {
        self.readme_scroll = self.readme_scroll.saturating_sub(n);
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

    pub fn draw(&mut self, f: &mut Frame) {
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

        // ── Main: left (list) + right (readme) ──
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(chunks[1]);

        // Left: plugin list
        let rows: Vec<Row> = self
            .plugins
            .iter()
            .map(|repo| {
                let desc = repo.description.as_deref().unwrap_or("");
                let desc_truncated: String = desc.chars().take(40).collect();
                let installed_cell = if self.is_installed(repo) {
                    ratatui::widgets::Cell::from(Span::styled(
                        "\u{2713}",
                        Style::default().fg(Color::Green),
                    ))
                } else {
                    ratatui::widgets::Cell::from(" ")
                };
                let topics_str: String = repo
                    .topics
                    .iter()
                    .take(3)
                    .map(|t| format!("#{}", t))
                    .collect::<Vec<_>>()
                    .join(" ");
                Row::new(vec![
                    installed_cell,
                    ratatui::widgets::Cell::from(format!(" \u{2605}{}", repo.stars_display()))
                        .style(Style::default().fg(Color::Yellow)),
                    ratatui::widgets::Cell::from(repo.plugin_name().to_string())
                        .style(Style::default().fg(Color::White)),
                    ratatui::widgets::Cell::from(desc_truncated)
                        .style(Style::default().fg(Color::DarkGray)),
                    ratatui::widgets::Cell::from(topics_str)
                        .style(Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(2),
                Constraint::Length(8),
                Constraint::Length(30),
                Constraint::Min(20),
                Constraint::Length(24),
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
        let readme_body = if self.readme_loading {
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

        // 選択中 repo の topics を README 先頭に prepend (DarkGray、区切り線付き)
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

        // バッジや `<div>` 等の HTML タグは pulldown-cmark が HTML block として扱うので
        // 生テキストが残る。最低限、行頭の `<img ...>` / `<a ...>` / `</a>` / `<br>` 等を
        // 除去して見た目を整える。
        let cleaned_body = strip_common_html(&readme_body);
        let combined = format!("{}{}", topics_prefix, cleaned_body);
        let rendered = tui_markdown::from_str(&combined);

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
        state.scroll_readme_down(10);
        assert_eq!(state.readme_scroll, 10);
        state.scroll_readme_up(3);
        assert_eq!(state.readme_scroll, 7);
        state.scroll_readme_up(100);
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
}
