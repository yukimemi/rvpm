// `rvpm profile` の TUI レンダリング。
//
// 画面構成 (上から):
//   1. Banner: 全体平均起動時間 + runs + nvim version (大きく強調)
//   2. Phase breakdown: managed plugins / merged / runtime+loader+user の棒グラフ
//   3. Plugin table: total_self_ms 降順、バー付き、lazy/eager バッジ
//   4. Detail panel: 選択中プラグインの top ファイル一覧 (self_ms 降順)
//   5. Footer: キーヘルプ
//
// 色の指針 (視認性 + テンションのある画面):
//   - アクセント: Magenta (banner)
//   - ホット (遅い): Red → Yellow → Cyan → Green のグラデーション
//   - 擬似グループ ([merged] 等) は DarkGray で控えめに

use crate::profile::{ProfileReport, is_group_name};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Table, TableState,
    },
};

/// プラグインテーブルのソート軸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    SelfMs,
    SourcedMs,
    FileCount,
    Name,
}

impl SortKey {
    fn label(&self) -> &'static str {
        match self {
            SortKey::SelfMs => "self",
            SortKey::SourcedMs => "total",
            SortKey::FileCount => "files",
            SortKey::Name => "name",
        }
    }

    fn next(self) -> Self {
        match self {
            SortKey::SelfMs => SortKey::SourcedMs,
            SortKey::SourcedMs => SortKey::FileCount,
            SortKey::FileCount => SortKey::Name,
            SortKey::Name => SortKey::SelfMs,
        }
    }
}

/// TUI の内部状態。
struct ProfileTuiState {
    report: ProfileReport,
    sort_key: SortKey,
    hide_groups: bool,
    table_state: TableState,
    show_help: bool,
}

impl ProfileTuiState {
    fn new(report: ProfileReport) -> Self {
        let mut ts = TableState::default();
        ts.select(Some(0));
        Self {
            report,
            sort_key: SortKey::SelfMs,
            hide_groups: false,
            table_state: ts,
            show_help: false,
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        let mut idxs: Vec<usize> = self
            .report
            .plugins
            .iter()
            .enumerate()
            .filter(|(_, p)| !self.hide_groups || !is_group_name(&p.name))
            .map(|(i, _)| i)
            .collect();
        match self.sort_key {
            SortKey::SelfMs => idxs.sort_by(|&a, &b| {
                self.report.plugins[b]
                    .total_self_ms
                    .partial_cmp(&self.report.plugins[a].total_self_ms)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            SortKey::SourcedMs => idxs.sort_by(|&a, &b| {
                self.report.plugins[b]
                    .total_sourced_ms
                    .partial_cmp(&self.report.plugins[a].total_sourced_ms)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            SortKey::FileCount => idxs.sort_by(|&a, &b| {
                self.report.plugins[b]
                    .file_count
                    .cmp(&self.report.plugins[a].file_count)
            }),
            SortKey::Name => idxs
                .sort_by(|&a, &b| self.report.plugins[a].name.cmp(&self.report.plugins[b].name)),
        }
        idxs
    }

    fn selected_plugin_index(&self) -> Option<usize> {
        let vis = self.visible_indices();
        self.table_state.selected().and_then(|i| vis.get(i).copied())
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.visible_indices().len();
        if len == 0 {
            return;
        }
        let cur = self.table_state.selected().unwrap_or(0) as isize;
        let new = (cur + delta).rem_euclid(len as isize) as usize;
        self.table_state.select(Some(new));
    }

    fn go_top(&mut self) {
        if !self.visible_indices().is_empty() {
            self.table_state.select(Some(0));
        }
    }

    fn go_bottom(&mut self) {
        let len = self.visible_indices().len();
        if len > 0 {
            self.table_state.select(Some(len - 1));
        }
    }
}

/// エントリポイント: TUI を起動してユーザが q で終了するまでブロック。
pub fn run(report: ProfileReport) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let mut state = ProfileTuiState::new(report);
    let result = run_loop(&mut terminal, &mut state);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn run_loop(
    terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &mut ProfileTuiState,
) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, state))?;
        if event::poll(std::time::Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('j') | KeyCode::Down => state.move_by(1),
                KeyCode::Char('k') | KeyCode::Up => state.move_by(-1),
                KeyCode::Char('g') | KeyCode::Home => state.go_top(),
                KeyCode::Char('G') | KeyCode::End => state.go_bottom(),
                KeyCode::Char('s') => state.sort_key = state.sort_key.next(),
                KeyCode::Char('h') => state.hide_groups = !state.hide_groups,
                KeyCode::Char('?') => state.show_help = !state.show_help,
                _ => {}
            }
        }
    }
}

fn draw(f: &mut Frame, state: &ProfileTuiState) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // banner
            Constraint::Length(7), // phase breakdown
            Constraint::Min(6),    // plugin table
            Constraint::Length(8), // detail
            Constraint::Length(3), // footer
        ])
        .split(area);

    draw_banner(f, chunks[0], state);
    draw_phase_breakdown(f, chunks[1], state);
    draw_plugin_table(f, chunks[2], state);
    draw_detail(f, chunks[3], state);
    draw_footer(f, chunks[4], state);

    if state.show_help {
        draw_help_overlay(f, area);
    }
}

fn draw_banner(f: &mut Frame, area: Rect, state: &ProfileTuiState) {
    let total = state.report.total_startup_ms;
    let rating_color = startup_color(total);
    let rating = startup_rating(total);

    let nvim = state.report.nvim_version.as_deref().unwrap_or("nvim");

    let line = Line::from(vec![
        Span::styled(
            " \u{26a1} rvpm profile ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{:>7.1} ms", total),
            Style::default()
                .fg(rating_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(rating, Style::default().fg(rating_color)),
        Span::raw("   "),
        Span::styled(
            format!("avg of {} run{}", state.report.runs, if state.report.runs == 1 { "" } else { "s" }),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("   "),
        Span::styled(
            truncate(nvim, area.width.saturating_sub(50) as usize),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta));
    let widget = Paragraph::new(line).alignment(Alignment::Left).block(block);
    f.render_widget(widget, area);
}

fn draw_phase_breakdown(f: &mut Frame, area: Rect, state: &ProfileTuiState) {
    let (managed_ms, managed_lazy_ms, merged_ms, group_ms) = summary_totals(state);
    let total = state.report.total_startup_ms.max(1e-6);
    let inner_width = area.width.saturating_sub(4) as usize;
    let bar_width = inner_width.saturating_sub(32);

    let rows = vec![
        summary_row("Eager plugins", managed_ms, total, bar_width, Color::Green),
        summary_row("Lazy (pre-trigger)", managed_lazy_ms, total, bar_width, Color::Cyan),
        summary_row("Merged rtp", merged_ms, total, bar_width, Color::Yellow),
        summary_row("Runtime + loader", group_ms, total, bar_width, Color::Blue),
    ];

    let title = Line::from(vec![
        Span::styled(
            " phase breakdown ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "(% of total startup)",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));

    let widget = Paragraph::new(rows).block(block);
    f.render_widget(widget, area);
}

fn summary_row(label: &str, ms: f64, total: f64, bar_w: usize, color: Color) -> Line<'_> {
    let ratio = (ms / total).clamp(0.0, 1.0);
    let filled = (ratio * bar_w as f64).round() as usize;
    let bar: String = std::iter::repeat_n('\u{2588}', filled)
        .chain(std::iter::repeat_n('\u{2591}', bar_w.saturating_sub(filled)))
        .collect();
    Line::from(vec![
        Span::styled(format!(" {:<19}", label), Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{:>7.1} ms  ", ms),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(bar, Style::default().fg(color)),
        Span::styled(
            format!("  {:>4.1}%", ratio * 100.0),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

fn summary_totals(state: &ProfileTuiState) -> (f64, f64, f64, f64) {
    let mut eager = 0.0;
    let mut lazy = 0.0;
    let mut merged = 0.0;
    let mut group = 0.0;
    for p in &state.report.plugins {
        if p.is_managed {
            if p.lazy {
                lazy += p.total_self_ms;
            } else {
                eager += p.total_self_ms;
            }
        } else if p.name == crate::profile::GROUP_MERGED {
            merged += p.total_self_ms;
        } else {
            group += p.total_self_ms;
        }
    }
    (eager, lazy, merged, group)
}

fn draw_plugin_table(f: &mut Frame, area: Rect, state: &ProfileTuiState) {
    let vis = state.visible_indices();
    let max_ms = vis
        .iter()
        .map(|&i| state.report.plugins[i].total_self_ms)
        .fold(0.0_f64, f64::max)
        .max(1e-6);

    let inner_w = area.width.saturating_sub(2) as usize;
    // name (26) + badge (5) + self (10) + total (10) + files (6) + margins
    let bar_w = inner_w.saturating_sub(60).max(6);

    let header_row = Row::new(vec![
        Cell::from(Span::styled(
            " # ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            "kind ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            "plugin",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            "self ms",
            sort_header_style(state.sort_key == SortKey::SelfMs),
        )),
        Cell::from(Span::styled(
            "total ms",
            sort_header_style(state.sort_key == SortKey::SourcedMs),
        )),
        Cell::from(Span::styled(
            "files",
            sort_header_style(state.sort_key == SortKey::FileCount),
        )),
        Cell::from(Span::styled(
            "distribution",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
    ])
    .height(1);

    let rows: Vec<Row> = vis
        .iter()
        .enumerate()
        .map(|(visual_i, &plugin_i)| {
            let p = &state.report.plugins[plugin_i];
            let is_group = is_group_name(&p.name);
            let name_color = if is_group {
                Color::DarkGray
            } else {
                Color::White
            };
            let (badge, badge_color) = plugin_badge(p);

            let bar_color = bar_color(p.total_self_ms, max_ms);
            let filled = ((p.total_self_ms / max_ms) * bar_w as f64).round() as usize;
            let bar: String = std::iter::repeat_n('\u{2588}', filled)
                .chain(std::iter::repeat_n('\u{2591}', bar_w.saturating_sub(filled)))
                .collect();

            Row::new(vec![
                Cell::from(Span::styled(
                    format!("{:>3}", visual_i + 1),
                    Style::default().fg(Color::DarkGray),
                )),
                Cell::from(Span::styled(
                    format!(" {} ", badge),
                    Style::default()
                        .fg(badge_color)
                        .add_modifier(Modifier::BOLD),
                )),
                Cell::from(Span::styled(
                    truncate(&p.name, 28),
                    Style::default().fg(name_color),
                )),
                Cell::from(Span::styled(
                    format!("{:>7.2}", p.total_self_ms),
                    Style::default()
                        .fg(bar_color)
                        .add_modifier(Modifier::BOLD),
                )),
                Cell::from(Span::styled(
                    format!("{:>7.2}", p.total_sourced_ms),
                    Style::default().fg(Color::Gray),
                )),
                Cell::from(Span::styled(
                    format!("{:>4}", p.file_count),
                    Style::default().fg(Color::DarkGray),
                )),
                Cell::from(Span::styled(bar, Style::default().fg(bar_color))),
            ])
        })
        .collect();

    let title = Line::from(vec![
        Span::styled(
            " plugins ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "({} shown, sorted by {}{})  ",
                vis.len(),
                state.sort_key.label(),
                if state.hide_groups { ", groups hidden" } else { "" }
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(30),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Min(6),
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("\u{25b6} ");

    // mutable TableState を渡すため copy (TableState は Copy)。描画用の transient state として問題なし。
    let mut ts = state.table_state;
    f.render_stateful_widget(table, area, &mut ts);

    // Scrollbar: 行数がエリアを越えたときの視覚的ヒント
    if vis.len() > area.height.saturating_sub(3) as usize {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        let mut sb_state = ScrollbarState::new(vis.len())
            .position(state.table_state.selected().unwrap_or(0));
        f.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb_state,
        );
    }
}

fn sort_header_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    }
}

fn plugin_badge(p: &crate::profile::PluginStats) -> (&'static str, Color) {
    if !p.is_managed {
        ("grp", Color::DarkGray)
    } else if p.lazy {
        ("lazy", Color::Cyan)
    } else {
        ("eagr", Color::Green)
    }
}

fn draw_detail(f: &mut Frame, area: Rect, state: &ProfileTuiState) {
    let Some(idx) = state.selected_plugin_index() else {
        return;
    };
    let p = &state.report.plugins[idx];

    let max_file_ms = p
        .top_files
        .iter()
        .map(|f| f.self_ms)
        .fold(0.0_f64, f64::max)
        .max(1e-6);

    let bar_w = (area.width.saturating_sub(60) as usize).max(4);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(" files ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("({} sourced)", p.file_count),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    if p.top_files.is_empty() {
        lines.push(Line::from(Span::styled(
            " (no sourced files recorded)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (i, file) in p.top_files.iter().take(5).enumerate() {
        let filled = ((file.self_ms / max_file_ms) * bar_w as f64).round() as usize;
        let bar: String = std::iter::repeat_n('\u{2588}', filled)
            .chain(std::iter::repeat_n('\u{2591}', bar_w.saturating_sub(filled)))
            .collect();
        let color = bar_color(file.self_ms, max_file_ms);
        lines.push(Line::from(vec![
            Span::styled(format!(" {:>2}. ", i + 1), Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncate(&file.relative_path, 34),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                format!("  {:>6.2} ms  ", file.self_ms),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(bar, Style::default().fg(color)),
        ]));
    }

    let title = Line::from(vec![
        Span::styled(
            format!(" {} ", p.name),
            Style::default()
                .fg(Color::Black)
                .bg(bar_color(p.total_self_ms, state.report.total_startup_ms.max(1.0)))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {:.2} ms self  /  {:.2} ms total", p.total_self_ms, p.total_sourced_ms),
            Style::default().fg(Color::Gray),
        ),
    ]);
    let widget = Paragraph::new(lines).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(widget, area);
}

fn draw_footer(f: &mut Frame, area: Rect, _state: &ProfileTuiState) {
    let mut spans: Vec<Span> = Vec::new();
    for (k, d) in [
        ("j/k", "move"),
        ("g/G", "top/bot"),
        ("s", "sort"),
        ("h", "hide groups"),
        ("?", "help"),
        ("q", "quit"),
    ] {
        spans.extend(key_hint(k, d));
    }
    let widget = Paragraph::new(Line::from(spans)).alignment(Alignment::Center).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(widget, area);
}

fn key_hint(key: &'static str, desc: &'static str) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!(" {} ", key),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {}  ", desc), Style::default().fg(Color::Gray)),
    ]
}

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let w = 54.min(area.width.saturating_sub(4));
    let h = 14.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);
    let lines = vec![
        Line::from(Span::styled(
            "  rvpm profile — keys",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  j / k / ↑ / ↓   move selection"),
        Line::from("  g / G           jump to top / bottom"),
        Line::from("  s               cycle sort (self → total → files → name)"),
        Line::from("  h               toggle [merged]/[runtime] group rows"),
        Line::from("  ?               toggle this help"),
        Line::from("  q / Esc         quit"),
        Line::from(""),
        Line::from(Span::styled(
            "  self = time spent in this plugin's own files",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  total = self + children (requires / source chain)",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let widget = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Magenta)),
    );
    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_widget(widget, rect);
}

/// 時間帯に応じた色: 小さいほど緑、大きいほど赤 (max 比で 4 段階)。
fn bar_color(ms: f64, max_ms: f64) -> Color {
    let ratio = (ms / max_ms).clamp(0.0, 1.0);
    if ratio < 0.25 {
        Color::Green
    } else if ratio < 0.5 {
        Color::Cyan
    } else if ratio < 0.75 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// 総起動時間の感覚的評価。
fn startup_rating(total_ms: f64) -> &'static str {
    if total_ms < 100.0 {
        "blazing \u{1f525}"
    } else if total_ms < 200.0 {
        "fast"
    } else if total_ms < 400.0 {
        "ok"
    } else {
        "slow \u{26a0}"
    }
}

fn startup_color(total_ms: f64) -> Color {
    if total_ms < 100.0 {
        Color::Green
    } else if total_ms < 200.0 {
        Color::Cyan
    } else if total_ms < 400.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// 長い文字列を末尾 `…` で切り詰める。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// `--no-tui` 用の plain text 出力。pipe-friendly。
pub fn print_plain(report: &ProfileReport, top: Option<usize>) {
    println!("# rvpm profile");
    println!(
        "total_startup_ms = {:.2}   runs = {}   nvim = {}",
        report.total_startup_ms,
        report.runs,
        report.nvim_version.as_deref().unwrap_or("(unknown)")
    );
    println!();
    println!(
        "  {:>4}  {:>4}  {:<30}  {:>10}  {:>10}  {:>6}",
        "#", "kind", "plugin", "self ms", "total ms", "files"
    );
    println!("  {}", "-".repeat(72));
    for (i, p) in report
        .plugins
        .iter()
        .take(top.unwrap_or(usize::MAX))
        .enumerate()
    {
        let kind = if !p.is_managed {
            "grp"
        } else if p.lazy {
            "lazy"
        } else {
            "eagr"
        };
        println!(
            "  {:>4}  {:>4}  {:<30}  {:>10.2}  {:>10.2}  {:>6}",
            i + 1,
            kind,
            truncate(&p.name, 30),
            p.total_self_ms,
            p.total_sourced_ms,
            p.file_count,
        );
    }
}
