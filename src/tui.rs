use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Gauge, List, ListItem, Paragraph, Row, Table, TableState},
};
use std::collections::HashMap;

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

    pub fn update_status(&mut self, url: &str, status: PluginStatus) {
        if let Some(s) = self.status_map.get_mut(url) {
            *s = status;
        }
    }

    pub fn draw(&self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(f.area());

        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                " R V P M ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  [Processing...]", Style::default().fg(Color::Cyan)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(title, chunks[0]);

        let items: Vec<ListItem> = self
            .plugins
            .iter()
            .map(|url| {
                let status = self
                    .status_map
                    .get(url)
                    .cloned()
                    .unwrap_or(PluginStatus::Waiting);
                let (icon, color, msg) = match &status {
                    PluginStatus::Waiting => {
                        ("\u{f0292}", Color::DarkGray, "Waiting...".to_string())
                    }
                    PluginStatus::Syncing(m) => ("\u{21bb}", Color::Cyan, m.clone()),
                    PluginStatus::Finished => ("\u{f00c}", Color::Green, "Finished".to_string()),
                    PluginStatus::Failed(e) => ("\u{2716}", Color::Red, e.clone()),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {} ", icon), Style::default().fg(color)),
                    Span::styled(format!("{:<40}", url), Style::default().fg(Color::White)),
                    Span::styled(msg, Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .title(" Plugins ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta)),
        );
        f.render_widget(list, chunks[1]);

        let finished_count = self
            .status_map
            .values()
            .filter(|s| matches!(s, PluginStatus::Finished))
            .count();
        let ratio = if !self.plugins.is_empty() {
            finished_count as f64 / self.plugins.len() as f64
        } else {
            1.0
        };
        let gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL))
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(ratio);
        f.render_widget(gauge, chunks[2]);
    }

    pub fn draw_list(
        &mut self,
        f: &mut Frame,
        config: &crate::config::Config,
        config_root: &std::path::Path,
    ) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(f.area());

        // インストール済み / 未インストール / エラー のカウント
        let installed = self
            .status_map
            .values()
            .filter(|s| matches!(s, PluginStatus::Finished))
            .count();
        let missing = self
            .status_map
            .values()
            .filter(|s| matches!(s, PluginStatus::Failed(m) if m == "Missing"))
            .count();
        let errors = self
            .status_map
            .values()
            .filter(|s| matches!(s, PluginStatus::Failed(m) if m != "Missing"))
            .count();
        let modified = self
            .status_map
            .values()
            .filter(|s| matches!(s, PluginStatus::Syncing(_)))
            .count();

        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                " P L U G I N   L I S T ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  Total:{} ", config.plugins.len()),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!(" \u{f00c}:{} ", installed),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!(" \u{f05e}:{} ", missing),
                Style::default().fg(Color::Red),
            ),
            Span::styled(
                format!(" \u{f071}:{} ", modified),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!(" \u{2716}:{} ", errors),
                Style::default().fg(Color::Red),
            ),
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
                    PluginStatus::Finished => ("\u{f00c}", Color::Green), //
                    PluginStatus::Failed(m) if m == "Missing" => ("\u{f05e}", Color::Red), //
                    PluginStatus::Failed(_) => ("\u{2716}", Color::Red),  // ✖
                    PluginStatus::Syncing(m) if m.contains("Modified") => {
                        ("\u{f071}", Color::Yellow)
                    } //
                    PluginStatus::Syncing(_) => ("\u{21bb}", Color::Cyan), // ↻
                    PluginStatus::Waiting => ("?", Color::DarkGray),
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

                let mode = if p.lazy {
                    ("Lazy", Color::Yellow)
                } else {
                    ("Eager", Color::Green)
                };
                let merged = if p.merge {
                    ("\u{f00c}", Color::Cyan)
                } else {
                    ("\u{2716}", Color::DarkGray)
                };
                let rev = p.rev.as_deref().unwrap_or("-");

                // I B A 列: init/before/after.lua の存在チェック
                let pcdir = config_root.join(p.canonical_path());
                let hook_i = if pcdir.join("init.lua").exists() {
                    "\u{25cf}"
                } else {
                    "\u{25cb}"
                };
                let hook_b = if pcdir.join("before.lua").exists() {
                    "\u{25cf}"
                } else {
                    "\u{25cb}"
                };
                let hook_a = if pcdir.join("after.lua").exists() {
                    "\u{25cf}"
                } else {
                    "\u{25cb}"
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
                Span::styled(
                    " [q] Quit ",
                    Style::default().fg(Color::Black).bg(Color::DarkGray),
                ),
                Span::styled(
                    " [j/k] Move ",
                    Style::default().fg(Color::Black).bg(Color::DarkGray),
                ),
                Span::styled(
                    " [g/G] Top/End ",
                    Style::default().fg(Color::Black).bg(Color::DarkGray),
                ),
                Span::styled(
                    " [/] Search [n/N] ",
                    Style::default().fg(Color::Black).bg(Color::DarkGray),
                ),
                Span::styled(
                    " [e] Edit ",
                    Style::default().fg(Color::Black).bg(Color::Magenta),
                ),
                Span::styled(
                    " [s] Set ",
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                ),
                Span::styled(
                    " [S] Sync ",
                    Style::default().fg(Color::Black).bg(Color::Green),
                ),
                Span::styled(
                    " [u/U] Update ",
                    Style::default().fg(Color::Black).bg(Color::Yellow),
                ),
                Span::styled(
                    " [d] Delete ",
                    Style::default().fg(Color::Black).bg(Color::Red),
                ),
            ]))
            .block(Block::default().borders(Borders::ALL))
        };
        f.render_widget(footer, chunks[2]);
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
}
