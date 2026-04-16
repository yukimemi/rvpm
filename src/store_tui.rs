use crate::store::GitHubRepo;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, TableState, Wrap},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Readme,
}

pub struct StoreTuiState {
    pub plugins: Vec<GitHubRepo>,
    pub table_state: TableState,
    pub search_mode: bool,
    pub search_input: String,
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

impl StoreTuiState {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            table_state: TableState::default(),
            search_mode: false,
            search_input: String::new(),
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
        let title_content = if self.search_mode {
            let match_info = format!(" ({} results)", self.plugins.len());
            Line::from(vec![
                Span::styled(
                    " rvpm store ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " / ",
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
                Row::new(vec![
                    ratatui::widgets::Cell::from(format!(" \u{2605}{}", repo.stars_display()))
                        .style(Style::default().fg(Color::Yellow)),
                    ratatui::widgets::Cell::from(repo.plugin_name().to_string())
                        .style(Style::default().fg(Color::White)),
                    ratatui::widgets::Cell::from(desc_truncated)
                        .style(Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(8),
                Constraint::Length(30),
                Constraint::Min(10),
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

        // Right: README preview
        let readme_text = if self.readme_loading {
            "Loading README...".to_string()
        } else {
            self.readme_content.clone().unwrap_or_else(|| {
                if self.plugins.is_empty() {
                    "Press / to search for plugins".to_string()
                } else {
                    "Loading...".to_string()
                }
            })
        };

        let readme_title = self
            .selected_repo()
            .map(|r| format!(" {} ", r.full_name))
            .unwrap_or_else(|| " README ".to_string());

        let readme = Paragraph::new(readme_text)
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
        let footer = if self.search_mode {
            Paragraph::new(Line::from(vec![
                Span::styled(" Enter", Style::default().fg(Color::Yellow)),
                Span::styled(":search ", Style::default().fg(Color::DarkGray)),
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
                Span::styled("Tab", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!(":{} ", focus_label),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::styled(":add ", Style::default().fg(Color::DarkGray)),
                Span::styled("s", Style::default().fg(Color::Yellow)),
                Span::styled(":sort ", Style::default().fg(Color::DarkGray)),
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
            let popup_w = 50u16.min(area.width.saturating_sub(4));
            let popup_h = 18u16.min(area.height.saturating_sub(4));
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
                Line::from(vec![
                    Span::styled("  /           ", Style::default().fg(Color::Yellow)),
                    Span::styled("Search plugins", Style::default().fg(Color::White)),
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
}
