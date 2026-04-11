use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStatus {
    Waiting,
    Syncing(String), // 進捗メッセージ
    Finished,
    Failed(String),
}

pub struct TuiState {
    pub plugins: Vec<String>,
    pub status_map: HashMap<String, PluginStatus>,
}

impl TuiState {
    pub fn new(plugin_urls: Vec<String>) -> Self {
        let mut status_map = HashMap::new();
        for url in &plugin_urls {
            status_map.insert(url.clone(), PluginStatus::Waiting);
        }
        Self { plugins: plugin_urls, status_map }
    }

    pub fn update_status(&mut self, url: &str, status: PluginStatus) {
        if let Some(s) = self.status_map.get_mut(url) {
            *s = status;
        }
    }
}

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Gauge},
    Frame,
};

impl TuiState {
    pub fn draw(&self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Title
                Constraint::Min(10),   // List
                Constraint::Length(3), // Summary/Status
            ])
            .split(f.size());

        // 1. Title (Cyber Style)
        let title = Paragraph::new(Line::from(vec![
            Span::styled(" R V P M ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled("  [Rust Vim Plugin Manager]", Style::default().fg(Color::Cyan)),
        ]))
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(title, chunks[0]);

        // 2. Plugin List
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

        let list = List::new(items)
            .block(Block::default()
                .title(Span::styled(" Plugins ", Style::default().fg(Color::Magenta)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta)));
        f.render_widget(list, chunks[1]);

        // 3. Overall Summary (Gauge)
        let finished_count = self.status_map.values().filter(|s| matches!(s, PluginStatus::Finished)).count();
        let total_count = self.plugins.len();
        let ratio = if total_count > 0 { finished_count as f64 / total_count as f64 } else { 1.0 };

        let gauge = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(" Overall Progress "))
            .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Black))
            .ratio(ratio)
            .label(format!("{} / {}", finished_count, total_count));
        f.render_widget(gauge, chunks[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tui_state_update() {
        let mut state = TuiState::new(vec!["repo1".to_string(), "repo2".to_string()]);
        assert_eq!(state.status_map["repo1"], PluginStatus::Waiting);

        state.update_status("repo1", PluginStatus::Syncing("Cloning...".to_string()));
        assert_eq!(state.status_map["repo1"], PluginStatus::Syncing("Cloning...".to_string()));
    }
}
