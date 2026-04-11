use std::collections::HashMap;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Gauge, Table, Row, Cell, TableState},
    Frame,
};

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
        Self { plugins: plugin_urls, status_map, table_state }
    }

    pub fn next(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => if i >= self.plugins.len() - 1 { 0 } else { i + 1 },
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn previous(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => if i == 0 { self.plugins.len() - 1 } else { i - 1 },
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn selected_url(&self) -> Option<String> {
        self.table_state.selected().map(|i| self.plugins[i].clone())
    }

    pub fn update_status(&mut self, url: &str, status: PluginStatus) {
        if let Some(s) = self.status_map.get_mut(url) {
            *s = status;
        }
    }

    pub fn draw(&self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(10), Constraint::Length(3)])
            .split(f.size());

        let title = Paragraph::new(Line::from(vec![
            Span::styled(" R V P M ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled("  [Processing...]", Style::default().fg(Color::Cyan)),
        ])).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(title, chunks[0]);

        let items: Vec<ListItem> = self.plugins.iter().map(|url| {
            let status = self.status_map.get(url).cloned().unwrap_or(PluginStatus::Waiting);
            let (icon, color, msg) = match &status {
                PluginStatus::Waiting => ("󰒲", Color::DarkGray, "Waiting...".to_string()),
                PluginStatus::Syncing(m) => ("↻", Color::Cyan, m.clone()),
                PluginStatus::Finished => ("", Color::Green, "Finished".to_string()),
                PluginStatus::Failed(e) => ("✖", Color::Red, e.clone()),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", icon), Style::default().fg(color)),
                Span::styled(format!("{:<40}", url), Style::default().fg(Color::White)),
                Span::styled(msg, Style::default().fg(Color::DarkGray)),
            ]))
        }).collect();

        let list = List::new(items).block(Block::default().title(" Plugins ").borders(Borders::ALL).border_style(Style::default().fg(Color::Magenta)));
        f.render_widget(list, chunks[1]);

        let finished_count = self.status_map.values().filter(|s| matches!(s, PluginStatus::Finished)).count();
        let ratio = if !self.plugins.is_empty() { finished_count as f64 / self.plugins.len() as f64 } else { 1.0 };
        let gauge = Gauge::default().block(Block::default().borders(Borders::ALL)).gauge_style(Style::default().fg(Color::Cyan)).ratio(ratio);
        f.render_widget(gauge, chunks[2]);
    }

    pub fn draw_list(&mut self, f: &mut Frame, config: &crate::config::Config) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(10), Constraint::Length(3)])
            .split(f.size());

        let title = Paragraph::new(Line::from(vec![
            Span::styled(" P L U G I N   L I S T ", Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  Total: {}", config.plugins.len()), Style::default().fg(Color::Magenta)),
        ])).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(title, chunks[0]);

        let header = Row::new(vec!["Plugin URL", "Status", "Merged", "Rev", "Triggers"].into_iter().map(|h| Cell::from(h).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))).style(Style::default().bg(Color::Black)).height(1).bottom_margin(1);
        
        let rows = config.plugins.iter().map(|p| {
            let status = if p.lazy { ("󰒲 Lazy", Color::Yellow) } else { ("󰚰 Eager", Color::Green) };
            let merged = if p.merge { ("", Color::Cyan) } else { ("✖", Color::DarkGray) };
            let rev = p.rev.as_deref().unwrap_or("-");
            
            let mut trg = Vec::new();
            if let Some(c) = &p.on_cmd { trg.push(format!("Cmd:{}", c.len())); }
            if let Some(f) = &p.on_ft { trg.push(format!("Ft:{}", f.len())); }
            if p.cond.is_some() { trg.push("Cond".to_string()); }

            Row::new(vec![
                Cell::from(p.url.clone()).style(Style::default().fg(Color::White)),
                Cell::from(status.0).style(Style::default().fg(status.1)),
                Cell::from(merged.0).style(Style::default().fg(merged.1)),
                Cell::from(rev).style(Style::default().fg(Color::Magenta)),
                Cell::from(trg.join(", ")).style(Style::default().fg(Color::DarkGray))
            ])
        });

        let table = Table::new(rows, [
            Constraint::Percentage(35),
            Constraint::Percentage(10),
            Constraint::Percentage(10),
            Constraint::Percentage(15),
            Constraint::Percentage(30)
        ])
            .header(header).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)).highlight_symbol(">> ");
        f.render_stateful_widget(table, chunks[1], &mut self.table_state);

        let footer = Paragraph::new(Line::from(vec![
            Span::styled(" [q] Quit ", Style::default().fg(Color::Black).bg(Color::DarkGray)),
            Span::styled(" [j/k] Move ", Style::default().fg(Color::Black).bg(Color::DarkGray)),
            Span::styled(" [e] Edit ", Style::default().fg(Color::Black).bg(Color::Magenta)),
            Span::styled(" [s] Set ", Style::default().fg(Color::Black).bg(Color::Cyan)),
        ])).block(Block::default().borders(Borders::ALL));
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
        assert_eq!(state.status_map["repo1"], PluginStatus::Syncing("Cloning...".to_string()));
    }

    #[test]
    fn test_plugin_status_colors() {
        // 表示ロジックのユニットテストは難しいので、状態の保持をテスト
        let mut state = TuiState::new(vec!["test".to_string()]);
        state.update_status("test", PluginStatus::Failed("Error".to_string()));
        assert!(matches!(state.status_map["test"], PluginStatus::Failed(_)));
    }
}
