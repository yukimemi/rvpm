use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, TableState},
};
use std::collections::HashMap;
use std::time::Instant;

/// Sync の UI で Syncing 状態の行に使う braille スピナーのフレーム。
/// 80ms 毎に次のフレームへ (12.5fps 程度)。
const SPINNER_BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// ASCII 環境向けのフォールバック。
const SPINNER_ASCII: &[&str] = &["|", "/", "-", "\\"];
/// Title 部分の "syncing..." の末尾ドット。3 フレームで循環。
const DOTS: &[&str] = &[".  ", ".. ", "..."];
/// スピナー / ドットのフレーム進行速度 (ms/frame)。
const FRAME_MS: u128 = 80;
const DOTS_MS: u128 = 400;

/// TUI で使用するアイコンセット。IconStyle に応じて切り替える。
pub struct Icons {
    pub waiting: &'static str,
    pub syncing: &'static str,
    pub finished: &'static str,
    pub failed: &'static str,
    /// list TUI 用
    pub installed: &'static str,
    pub missing: &'static str,
    pub modified: &'static str,
    pub hook_on: &'static str,
    pub hook_off: &'static str,
    /// スピナーのフレームセットを選ぶためにスタイル情報を保持しておく。
    pub style: crate::config::IconStyle,
}

impl Icons {
    pub fn from_style(style: crate::config::IconStyle) -> Self {
        match style {
            crate::config::IconStyle::Nerd => Self {
                waiting: "\u{f0292}",  // 󰊒
                syncing: "\u{21bb}",   // ↻
                finished: "\u{f00c}",  //
                failed: "\u{2716}",    // ✖
                installed: "\u{f00c}", //
                missing: "\u{f05e}",   //
                modified: "\u{f071}",  //
                hook_on: "\u{25cf}",   // ●
                hook_off: "\u{25cb}",  // ○
                style,
            },
            crate::config::IconStyle::Unicode => Self {
                waiting: "\u{25cb}",   // ○
                syncing: "\u{21bb}",   // ↻
                finished: "\u{2713}",  // ✓
                failed: "\u{2717}",    // ✗
                installed: "\u{2713}", // ✓
                missing: "\u{2718}",   // ✘
                modified: "\u{26a0}",  // ⚠
                hook_on: "\u{25cf}",   // ●
                hook_off: "\u{25cb}",  // ○
                style,
            },
            crate::config::IconStyle::Ascii => Self {
                waiting: ".",
                syncing: "*",
                finished: "+",
                failed: "x",
                installed: "+",
                missing: "!",
                modified: "~",
                hook_on: "o",
                hook_off: "-",
                style,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStatus {
    Waiting,
    Syncing(String),
    Finished,
    Failed(String),
}

pub struct TuiState {
    pub plugins: Vec<String>,
    pub status_map: HashMap<String, PluginStatus>,
    pub table_state: TableState,
    /// `/` 検索のパターン
    pub search_pattern: Option<String>,
    /// 検索にヒットしたインデックス一覧 (ソート済み)
    pub search_matches: Vec<usize>,
    /// 検索の現在位置 (search_matches 内のインデックス)
    pub search_cursor: usize,
    /// 検索モード (TUI 内インライン検索)
    pub search_mode: bool,
    /// 検索モード中の入力バッファ
    pub search_input: String,
    /// ヘルプ表示中
    pub show_help: bool,
    /// TUI 起動時刻。Syncing スピナーや経過時間表示の基準にする。
    pub started_at: Instant,
}

impl TuiState {
    pub fn new(plugin_urls: Vec<String>) -> Self {
        let mut status_map = HashMap::new();
        for url in &plugin_urls {
            status_map.insert(url.clone(), PluginStatus::Waiting);
        }
        let mut table_state = TableState::default();
        if !plugin_urls.is_empty() {
            table_state.select(Some(0));
        }
        Self {
            plugins: plugin_urls,
            status_map,
            table_state,
            search_pattern: None,
            search_matches: Vec::new(),
            search_cursor: 0,
            search_mode: false,
            search_input: String::new(),
            show_help: false,
            started_at: Instant::now(),
        }
    }

    /// スピナー選択用のミリ秒基準 tick。`Instant::now().elapsed()` に依存せず
    /// 関数型テスト可能 (タイムスタンプをモック出来る) にしたいときは `u128`
    /// を直接渡せる下記のような実装にすると楽。ここではシンプルに経過時間を使う。
    fn elapsed_ms(&self) -> u128 {
        self.started_at.elapsed().as_millis()
    }

    /// Sync UI の title に出す経過時間 (`mm:ss`)。
    fn elapsed_str(&self) -> String {
        let s = self.started_at.elapsed().as_secs();
        format!("{:02}:{:02}", s / 60, s % 60)
    }

    /// IconStyle に応じてスピナーフレームを返す。時間ベースで 80ms ごとに次の
    /// フレームに進む (再描画頻度に依存しない、見た目が一定)。
    fn spinner_frame(&self, style: crate::config::IconStyle) -> &'static str {
        let frames: &[&str] = match style {
            crate::config::IconStyle::Nerd | crate::config::IconStyle::Unicode => SPINNER_BRAILLE,
            crate::config::IconStyle::Ascii => SPINNER_ASCII,
        };
        let idx = (self.elapsed_ms() / FRAME_MS) as usize % frames.len();
        frames[idx]
    }

    /// "syncing." "syncing.." "syncing..." を循環させる用のドット部分。
    fn dots_frame(&self) -> &'static str {
        let idx = (self.elapsed_ms() / DOTS_MS) as usize % DOTS.len();
        DOTS[idx]
    }

    /// progress ratio (0.0..=1.0) から段階的にゲージ色を決める。
    /// 0-25% 赤, -50% 黄, -75% シアン, それ以上は緑。
    fn progress_color(ratio: f64) -> Color {
        if ratio < 0.25 {
            Color::Red
        } else if ratio < 0.5 {
            Color::Yellow
        } else if ratio < 0.75 {
            Color::Cyan
        } else {
            Color::Green
        }
    }

    pub fn next(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => {
                if i >= self.plugins.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn previous(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.plugins.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn selected_url(&self) -> Option<String> {
        self.table_state.selected().map(|i| self.plugins[i].clone())
    }

    /// g — 先頭へ
    pub fn go_top(&mut self) {
        if !self.plugins.is_empty() {
            self.table_state.select(Some(0));
        }
    }

    /// G — 末尾へ
    pub fn go_bottom(&mut self) {
        if !self.plugins.is_empty() {
            self.table_state.select(Some(self.plugins.len() - 1));
        }
    }

    /// 指定行数だけ下へ移動 (末尾でクランプ)
    pub fn move_down(&mut self, n: usize) {
        if self.plugins.is_empty() {
            return;
        }
        let current = self.table_state.selected().unwrap_or(0);
        let target = (current + n).min(self.plugins.len() - 1);
        self.table_state.select(Some(target));
    }

    /// 指定行数だけ上へ移動 (先頭でクランプ)
    pub fn move_up(&mut self, n: usize) {
        let current = self.table_state.selected().unwrap_or(0);
        let target = current.saturating_sub(n);
        self.table_state.select(Some(target));
    }

    /// 検索を実行してマッチ一覧を更新。最初のマッチに移動。
    pub fn search(&mut self, pattern: &str) {
        let pat = pattern.to_lowercase();
        self.search_matches = self
            .plugins
            .iter()
            .enumerate()
            .filter(|(_, url)| url.to_lowercase().contains(&pat))
            .map(|(i, _)| i)
            .collect();
        self.search_pattern = Some(pattern.to_string());
        self.search_cursor = 0;
        if let Some(&idx) = self.search_matches.first() {
            self.table_state.select(Some(idx));
        }
    }

    /// n — 次の検索結果へ
    pub fn search_next(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_cursor = (self.search_cursor + 1) % self.search_matches.len();
        self.table_state
            .select(Some(self.search_matches[self.search_cursor]));
    }

    /// 検索モードを開始
    pub fn start_search(&mut self) {
        self.search_mode = true;
        self.search_input.clear();
    }

    /// 検索モードで文字を入力 (インクリメンタル)
    pub fn search_type(&mut self, c: char) {
        self.search_input.push(c);
        self.search(&self.search_input.clone());
    }

    /// 検索モードで Backspace
    pub fn search_backspace(&mut self) {
        self.search_input.pop();
        if self.search_input.is_empty() {
            self.search_matches.clear();
            self.search_pattern = None;
        } else {
            self.search(&self.search_input.clone());
        }
    }

    /// 検索モードを確定
    pub fn search_confirm(&mut self) {
        self.search_mode = false;
        // search_pattern は保持 (n/N で引き続き使える)
    }

    /// 検索モードをキャンセル
    pub fn search_cancel(&mut self) {
        self.search_mode = false;
        self.search_input.clear();
        self.search_matches.clear();
        self.search_pattern = None;
    }

    /// N — 前の検索結果へ
    pub fn search_prev(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_cursor = if self.search_cursor == 0 {
            self.search_matches.len() - 1
        } else {
            self.search_cursor - 1
        };
        self.table_state
            .select(Some(self.search_matches[self.search_cursor]));
    }

    /// sync/update 中にスクロール系キー入力を処理する。
    /// terminal_height はページ計算に使う。
    pub fn handle_scroll_key(&mut self, key: crossterm::event::KeyEvent, terminal_height: u16) {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return;
        }
        let half_page = (terminal_height as usize).saturating_sub(8) / 2;
        let full_page = half_page * 2;
        use crossterm::event::{KeyCode, KeyModifiers};
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.next(),
            KeyCode::Char('k') | KeyCode::Up => self.previous(),
            KeyCode::Char('g') | KeyCode::Home => self.go_top(),
            KeyCode::Char('G') | KeyCode::End => self.go_bottom(),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_down(half_page)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_up(half_page)
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_down(full_page)
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_up(full_page)
            }
            _ => {}
        }
    }

    pub fn update_status(&mut self, url: &str, status: PluginStatus) {
        if let Some(s) = self.status_map.get_mut(url) {
            *s = status;
        }
    }

    pub fn draw(&mut self, f: &mut Frame, message: &str, icons: &Icons) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(f.area());

        let finished_count = self
            .status_map
            .values()
            .filter(|s| matches!(s, PluginStatus::Finished))
            .count();
        let failed_count = self
            .status_map
            .values()
            .filter(|s| matches!(s, PluginStatus::Failed(_)))
            .count();
        let syncing_count = self
            .status_map
            .values()
            .filter(|s| matches!(s, PluginStatus::Syncing(_)))
            .count();
        // Finished と Failed は両方「処理済み」として ratio に含める。
        // Failed のみ残るパターンで永遠に 100% に届かない状態を防ぐ。
        let done_count = finished_count + failed_count;
        let ratio = if !self.plugins.is_empty() {
            done_count as f64 / self.plugins.len() as f64
        } else {
            1.0
        };
        let gauge_color = Self::progress_color(ratio);

        // "syncing..." を dots animation 付きで、末尾に mm:ss と N in flight を足す。
        let message_trim = message.trim_end_matches(['.', ' ']);
        let animated_msg = format!("{}{}", message_trim, self.dots_frame());

        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                " rvpm ",
                Style::default()
                    .fg(Color::Black)
                    .bg(gauge_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}  ", animated_msg),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                format!("{}", finished_count),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("/", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", self.plugins.len()),
                Style::default().fg(Color::White),
            ),
            if syncing_count > 0 {
                Span::styled(
                    format!("  {}{} ", self.spinner_frame(icons.style), syncing_count),
                    Style::default().fg(Color::Cyan),
                )
            } else {
                Span::raw("  ")
            },
            if failed_count > 0 {
                Span::styled(
                    format!(" {}err", failed_count),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("")
            },
            Span::styled("   ", Style::default()),
            Span::styled(
                format!("⏱ {}", self.elapsed_str()),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(title, chunks[0]);

        // URL 列幅をターミナル幅に合わせて制限 (icon:4 + status_msg:~20 + border:4)
        let available = chunks[1].width.saturating_sub(28) as usize;
        let max_url_len = self
            .plugins
            .iter()
            .map(|u| u.len())
            .max()
            .unwrap_or(20)
            .min(available);

        // Syncing 行は static な icons.syncing ではなく時間駆動の braille/ascii
        // スピナーを使う。URL 名自体を薄めに (Waiting) / ボールド (Syncing /
        // Failed) と段階的にハイライトして、どれがアクティブか視覚的に判別しやすくする。
        let spinner_char = self.spinner_frame(icons.style);
        let rows: Vec<Row> = self
            .plugins
            .iter()
            .map(|url| {
                let status = self
                    .status_map
                    .get(url)
                    .cloned()
                    .unwrap_or(PluginStatus::Waiting);
                let (icon, icon_color, url_style, msg, msg_color) = match &status {
                    PluginStatus::Waiting => (
                        icons.waiting,
                        Color::DarkGray,
                        Style::default().fg(Color::DarkGray),
                        "Waiting…".to_string(),
                        Color::DarkGray,
                    ),
                    PluginStatus::Syncing(m) => (
                        spinner_char,
                        Color::Cyan,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        m.clone(),
                        Color::Cyan,
                    ),
                    PluginStatus::Finished => (
                        icons.finished,
                        Color::Green,
                        Style::default().fg(Color::Gray),
                        "Finished".to_string(),
                        Color::DarkGray,
                    ),
                    PluginStatus::Failed(e) => (
                        icons.failed,
                        Color::Red,
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        e.clone(),
                        Color::Red,
                    ),
                };
                Row::new(vec![
                    Cell::from(format!(" {} ", icon))
                        .style(Style::default().fg(icon_color).add_modifier(Modifier::BOLD)),
                    Cell::from(url.as_str()).style(url_style),
                    Cell::from(msg).style(Style::default().fg(msg_color)),
                ])
            })
            .collect();

        // Plugins テーブル枠:
        //  - 失敗あり: 赤
        //  - 1 つでも sync 中: gauge_color (進行度に応じて赤→黄→シアン→緑)
        //  - 全 Waiting (ジョブまだ始まってない): DarkGray
        //  - 全 Finished: 緑
        let table_border_color = if failed_count > 0 {
            Color::Red
        } else if syncing_count > 0 {
            gauge_color
        } else if done_count == self.plugins.len() && !self.plugins.is_empty() {
            Color::Green
        } else {
            Color::DarkGray
        };
        let table = Table::new(
            rows,
            [
                Constraint::Length(4),
                Constraint::Length(max_url_len as u16),
                Constraint::Min(10),
            ],
        )
        .block(
            Block::default()
                .title(Line::from(vec![
                    Span::styled(
                        " Plugins ",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("({} in flight) ", syncing_count),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(table_border_color)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Indexed(237))
                .add_modifier(Modifier::BOLD),
        );
        f.render_stateful_widget(table, chunks[1], &mut self.table_state);

        // progress gauge: 色はプログレスでグラデーション、ラベルに x/y と percent。
        let percent = (ratio * 100.0).round() as u16;
        let gauge = Gauge::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(gauge_color)),
            )
            .gauge_style(
                Style::default()
                    .fg(gauge_color)
                    .add_modifier(Modifier::BOLD),
            )
            .label(Span::styled(
                // percent と揃えるため done_count (finished + failed) を使う。
                // finished だけだと failed がある時に `100%   8/9` のような
                // 矛盾した表示になる。
                format!("{:>3}%   {}/{}", percent, done_count, self.plugins.len()),
                Style::default()
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ))
            .ratio(ratio.clamp(0.0, 1.0));
        f.render_widget(gauge, chunks[2]);
    }

    pub fn draw_list(
        &mut self,
        f: &mut Frame,
        config: &crate::config::Config,
        config_root: &std::path::Path,
        icons: &Icons,
    ) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(f.area());

        let eager_count = config.plugins.iter().filter(|p| !p.lazy).count();
        let lazy_count = config.plugins.iter().filter(|p| p.lazy).count();
        let error_count = self
            .status_map
            .values()
            .filter(|s| matches!(s, PluginStatus::Failed(_)))
            .count();
        let modified_count = self
            .status_map
            .values()
            .filter(|s| matches!(s, PluginStatus::Syncing(_)))
            .count();

        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                " rvpm ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}:", config.plugins.len()),
                Style::default().fg(Color::White),
            ),
            Span::styled("total ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}:", eager_count),
                Style::default().fg(Color::Green),
            ),
            Span::styled("eager ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}:", lazy_count),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled("lazy ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}:", error_count), Style::default().fg(Color::Red)),
            Span::styled("err ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}:", modified_count),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled("mod", Style::default().fg(Color::DarkGray)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(title, chunks[0]);

        let header = Row::new(
            ["", "Plugin", "Mode", "Merge", "Rev", "I B A", "Detail"]
                .iter()
                .map(|h| {
                    Cell::from(*h).style(
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                }),
        )
        .style(Style::default().bg(Color::Black))
        .height(1)
        .bottom_margin(1);

        let rows: Vec<Row> = config
            .plugins
            .iter()
            .map(|p| {
                // インストール状態アイコン
                let install_status = self
                    .status_map
                    .get(&p.url)
                    .cloned()
                    .unwrap_or(PluginStatus::Waiting);
                let (inst_icon, inst_color) = match &install_status {
                    PluginStatus::Finished => (icons.installed, Color::Green),
                    PluginStatus::Failed(m) if m == "Missing" => (icons.missing, Color::Red),
                    PluginStatus::Failed(_) => (icons.failed, Color::Red),
                    PluginStatus::Syncing(m) if m.contains("Modified") => {
                        (icons.modified, Color::Yellow)
                    }
                    PluginStatus::Syncing(_) => (icons.syncing, Color::Cyan),
                    PluginStatus::Waiting => (icons.waiting, Color::DarkGray),
                };

                // 詳細列: エラー/変更時はその内容、正常時はトリガー情報
                let (detail_text, detail_color) = match &install_status {
                    PluginStatus::Finished => {
                        let mut trg = Vec::new();
                        if let Some(c) = &p.on_cmd {
                            trg.push(format!("cmd:{}", c.len()));
                        }
                        if let Some(f) = &p.on_ft {
                            trg.push(format!("ft:{}", f.len()));
                        }
                        if let Some(m) = &p.on_map {
                            trg.push(format!("map:{}", m.len()));
                        }
                        if let Some(e) = &p.on_event {
                            trg.push(format!("ev:{}", e.len()));
                        }
                        if let Some(s) = &p.on_source {
                            trg.push(format!("src:{}", s.len()));
                        }
                        if p.cond.is_some() {
                            trg.push("cond".to_string());
                        }
                        (trg.join(" "), Color::DarkGray)
                    }
                    PluginStatus::Failed(msg) => (msg.clone(), Color::Red),
                    PluginStatus::Syncing(msg) => (msg.clone(), Color::Yellow),
                    PluginStatus::Waiting => ("Checking...".to_string(), Color::DarkGray),
                };

                let mode = if p.dev {
                    ("Dev", Color::Magenta)
                } else if p.lazy {
                    ("Lazy", Color::Yellow)
                } else {
                    ("Eager", Color::Green)
                };
                let merged = if p.merge {
                    (icons.installed, Color::Cyan)
                } else {
                    ("-", Color::DarkGray)
                };
                let rev = p.rev.as_deref().unwrap_or("-");

                // I B A 列: init/before/after.lua の存在チェック
                // per-plugin hook は <config_root>/plugins/<host>/<owner>/<repo>/
                let pcdir = config_root.join("plugins").join(p.canonical_path());
                let hook_i = if pcdir.join("init.lua").exists() {
                    icons.hook_on
                } else {
                    icons.hook_off
                };
                let hook_b = if pcdir.join("before.lua").exists() {
                    icons.hook_on
                } else {
                    icons.hook_off
                };
                let hook_a = if pcdir.join("after.lua").exists() {
                    icons.hook_on
                } else {
                    icons.hook_off
                };
                let hooks_text = format!("{} {} {}", hook_i, hook_b, hook_a);
                let has_hooks = pcdir.join("init.lua").exists()
                    || pcdir.join("before.lua").exists()
                    || pcdir.join("after.lua").exists();
                let hooks_color = if has_hooks {
                    Color::Green
                } else {
                    Color::DarkGray
                };

                Row::new(vec![
                    Cell::from(inst_icon).style(Style::default().fg(inst_color)),
                    Cell::from(p.display_name()).style(Style::default().fg(Color::White)),
                    Cell::from(mode.0).style(Style::default().fg(mode.1)),
                    Cell::from(merged.0).style(Style::default().fg(merged.1)),
                    Cell::from(rev).style(Style::default().fg(Color::Magenta)),
                    Cell::from(hooks_text).style(Style::default().fg(hooks_color)),
                    Cell::from(detail_text).style(Style::default().fg(detail_color)),
                ])
            })
            .collect();

        // URL 列をコンテンツの最大長に合わせる (最小 20、最大 60)
        let name_col_w = config
            .plugins
            .iter()
            .map(|p| p.display_name().len())
            .max()
            .unwrap_or(20)
            .clamp(20, 60) as u16;
        // rev 列をコンテンツの最大長に合わせる (最小 3、最大 20)
        let rev_col_w = config
            .plugins
            .iter()
            .map(|p| p.rev.as_deref().unwrap_or("-").len())
            .max()
            .unwrap_or(3)
            .clamp(3, 20) as u16;

        let table = Table::new(
            rows,
            [
                Constraint::Length(3),          // アイコン
                Constraint::Length(name_col_w), // Plugin name (動的)
                Constraint::Length(6),          // Mode
                Constraint::Length(6),          // Merge
                Constraint::Length(rev_col_w),  // Rev (動的)
                Constraint::Length(7),          // I B A (hooks)
                Constraint::Min(10),            // Detail (残り全部)
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Indexed(237)) // #3a3a3a — 落ち着いたダークグレー
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("\u{25b8} "); // ▸
        f.render_stateful_widget(table, chunks[1], &mut self.table_state);

        let footer = if self.search_mode {
            // 検索モード: vim-like "/" プロンプト
            let match_info = if self.search_matches.is_empty() && !self.search_input.is_empty() {
                " (no match)".to_string()
            } else if !self.search_matches.is_empty() {
                format!(
                    " ({}/{})",
                    self.search_cursor + 1,
                    self.search_matches.len()
                )
            } else {
                String::new()
            };
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "/",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(&self.search_input, Style::default().fg(Color::White)),
                Span::styled(
                    "\u{2588}", // █ カーソル
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(match_info, Style::default().fg(Color::DarkGray)),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
        } else {
            Paragraph::new(Line::from(vec![
                Span::styled(" e", Style::default().fg(Color::Cyan)),
                Span::styled(":edit ", Style::default().fg(Color::DarkGray)),
                Span::styled("s", Style::default().fg(Color::Cyan)),
                Span::styled(":set ", Style::default().fg(Color::DarkGray)),
                Span::styled("S", Style::default().fg(Color::Cyan)),
                Span::styled(":sync ", Style::default().fg(Color::DarkGray)),
                Span::styled("u/U", Style::default().fg(Color::Cyan)),
                Span::styled(":update ", Style::default().fg(Color::DarkGray)),
                Span::styled("d", Style::default().fg(Color::Cyan)),
                Span::styled(":delete ", Style::default().fg(Color::DarkGray)),
                Span::styled("/", Style::default().fg(Color::Cyan)),
                Span::styled(":search ", Style::default().fg(Color::DarkGray)),
                Span::styled("?", Style::default().fg(Color::Cyan)),
                Span::styled(":help ", Style::default().fg(Color::DarkGray)),
                Span::styled("q", Style::default().fg(Color::Cyan)),
                Span::styled(":quit", Style::default().fg(Color::DarkGray)),
            ]))
            .block(Block::default().borders(Borders::ALL))
        };
        f.render_widget(footer, chunks[2]);

        // ── Help popup overlay ──
        if self.show_help {
            let area = f.area();
            let popup_w = 48u16.min(area.width.saturating_sub(4));
            let popup_h = 16u16.min(area.height.saturating_sub(4));
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
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  j / k       ", Style::default().fg(Color::Cyan)),
                    Span::styled("Move down / up", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  g / G       ", Style::default().fg(Color::Cyan)),
                    Span::styled("Go to top / bottom", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  C-d / C-u   ", Style::default().fg(Color::Cyan)),
                    Span::styled("Half page down / up", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  C-f / C-b   ", Style::default().fg(Color::Cyan)),
                    Span::styled("Full page down / up", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  / n N       ", Style::default().fg(Color::Cyan)),
                    Span::styled("Search / next / prev", Style::default().fg(Color::White)),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  Actions",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  e           ", Style::default().fg(Color::Cyan)),
                    Span::styled("Edit hooks", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  s           ", Style::default().fg(Color::Cyan)),
                    Span::styled("Set plugin options", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  S           ", Style::default().fg(Color::Cyan)),
                    Span::styled("Sync all", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  u / U       ", Style::default().fg(Color::Cyan)),
                    Span::styled("Update selected / all", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  d           ", Style::default().fg(Color::Cyan)),
                    Span::styled("Delete selected", Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  q / Esc     ", Style::default().fg(Color::Cyan)),
                    Span::styled("Quit", Style::default().fg(Color::White)),
                ]),
            ];

            f.render_widget(Clear, popup);
            f.render_widget(
                Paragraph::new(help_lines).block(
                    Block::default()
                        .title(" Help [?] ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan)),
                ),
                popup,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tui_state_update() {
        let mut state = TuiState::new(vec!["repo1".to_string(), "repo2".to_string()]);
        state.update_status("repo1", PluginStatus::Syncing("Cloning...".to_string()));
        assert_eq!(
            state.status_map["repo1"],
            PluginStatus::Syncing("Cloning...".to_string())
        );
    }

    #[test]
    fn test_plugin_status_colors() {
        // 表示ロジックのユニットテストは難しいので、状態の保持をテスト
        let mut state = TuiState::new(vec!["test".to_string()]);
        state.update_status("test", PluginStatus::Failed("Error".to_string()));
        assert!(matches!(state.status_map["test"], PluginStatus::Failed(_)));
    }

    #[test]
    fn test_progress_color_buckets() {
        assert_eq!(TuiState::progress_color(0.0), Color::Red);
        assert_eq!(TuiState::progress_color(0.24), Color::Red);
        assert_eq!(TuiState::progress_color(0.25), Color::Yellow);
        assert_eq!(TuiState::progress_color(0.49), Color::Yellow);
        assert_eq!(TuiState::progress_color(0.5), Color::Cyan);
        assert_eq!(TuiState::progress_color(0.74), Color::Cyan);
        assert_eq!(TuiState::progress_color(0.75), Color::Green);
        assert_eq!(TuiState::progress_color(1.0), Color::Green);
    }

    #[test]
    fn test_elapsed_str_format() {
        // 起動直後なら 00:00、60 秒経てば 01:00 の形で mm:ss を出す
        let state = TuiState::new(vec!["a".to_string()]);
        let s = state.elapsed_str();
        assert!(s.len() == 5 && s.as_bytes()[2] == b':', "got {}", s);
        // 時間は 00:00 前後 (テスト実行で消費するマイクロ秒は無視できる)
        assert_eq!(&s[0..3], "00:");
    }

    #[test]
    fn test_spinner_frame_varies_over_time() {
        use crate::config::IconStyle;
        // 直接内部を触れないので、同一インスタンスから 2 回連続で取っても常に SPINNER_BRAILLE の要素であることだけ確認
        let state = TuiState::new(vec!["a".to_string()]);
        let frame = state.spinner_frame(IconStyle::Nerd);
        assert!(
            SPINNER_BRAILLE.contains(&frame),
            "unexpected frame {}",
            frame
        );
        let ascii_frame = state.spinner_frame(IconStyle::Ascii);
        assert!(
            SPINNER_ASCII.contains(&ascii_frame),
            "ascii frame not in set: {}",
            ascii_frame
        );
    }

    #[test]
    fn test_dots_frame_cycles() {
        let state = TuiState::new(vec!["a".to_string()]);
        let d = state.dots_frame();
        assert!(DOTS.contains(&d));
    }

    #[test]
    fn test_install_status_icons() {
        // インストール状態ごとのステータスマッピングを確認
        let mut state = TuiState::new(vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ]);
        state.update_status("a", PluginStatus::Finished);
        state.update_status("b", PluginStatus::Failed("Missing".to_string()));
        state.update_status("c", PluginStatus::Syncing("Modified".to_string()));
        state.update_status("d", PluginStatus::Failed("git error".to_string()));

        assert!(matches!(state.status_map["a"], PluginStatus::Finished));
        assert!(matches!(&state.status_map["b"], PluginStatus::Failed(m) if m == "Missing"));
        assert!(
            matches!(&state.status_map["c"], PluginStatus::Syncing(m) if m.contains("Modified"))
        );
        assert!(matches!(&state.status_map["d"], PluginStatus::Failed(m) if m != "Missing"));
    }

    #[test]
    fn test_icons_nerd_uses_private_use_area() {
        let icons = Icons::from_style(crate::config::IconStyle::Nerd);
        assert!(icons.finished.contains('\u{f00c}'));
    }

    #[test]
    fn test_icons_unicode_uses_standard_symbols() {
        let icons = Icons::from_style(crate::config::IconStyle::Unicode);
        assert_eq!(icons.finished, "\u{2713}"); // ✓
        assert_eq!(icons.failed, "\u{2717}"); // ✗
        assert_eq!(icons.waiting, "\u{25cb}"); // ○
    }

    #[test]
    fn test_icons_ascii_uses_only_ascii() {
        let icons = Icons::from_style(crate::config::IconStyle::Ascii);
        assert!(icons.finished.is_ascii());
        assert!(icons.failed.is_ascii());
        assert!(icons.waiting.is_ascii());
        assert!(icons.syncing.is_ascii());
    }
}
