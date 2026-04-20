// `rvpm profile` の TUI レンダリング。
//
// 画面構成 (上から):
//   1. Banner: 全体平均起動時間 + runs + nvim version + mode バッジ
//   2. Phase timeline: P3/P4/P5/P6/P7/P9 のバー (instrumented 時のみ)
//   3. Plugin table: kind / init / load / trig / total ms + distribution bar
//   4. Detail panel: 選択プラグインの phase 6 ファイル内訳 (sourcing 順)
//   5. Footer: キーヘルプ
//
// 色の指針:
//   - アクセント: Magenta (banner / keycaps)
//   - グラデーション: 速い = Green → Cyan → Yellow → Red = 遅い
//   - 擬似グループ ([merged] / [runtime] 等): DarkGray で控えめ

use crate::profile::{PhaseTime, ProfileReport, is_group_name};
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
    Load,
    Init,
    Trig,
    Total,
    Files,
    Name,
}

impl SortKey {
    fn label(&self) -> &'static str {
        match self {
            SortKey::Load => "load",
            SortKey::Init => "init",
            SortKey::Trig => "trig",
            SortKey::Total => "total",
            SortKey::Files => "files",
            SortKey::Name => "name",
        }
    }

    fn next(self) -> Self {
        match self {
            SortKey::Load => SortKey::Init,
            SortKey::Init => SortKey::Trig,
            SortKey::Trig => SortKey::Total,
            SortKey::Total => SortKey::Files,
            SortKey::Files => SortKey::Name,
            SortKey::Name => SortKey::Load,
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
        // instrumented の時は load、素計測時は total が自然
        let sort_key = if report.no_instrument {
            SortKey::Total
        } else {
            SortKey::Load
        };
        Self {
            report,
            sort_key,
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
        let ps = &self.report.plugins;
        match self.sort_key {
            SortKey::Load => idxs.sort_by(|&a, &b| cmp_f64(ps[b].load_ms, ps[a].load_ms)),
            SortKey::Init => idxs.sort_by(|&a, &b| cmp_f64(ps[b].init_ms, ps[a].init_ms)),
            SortKey::Trig => idxs.sort_by(|&a, &b| cmp_f64(ps[b].trig_ms, ps[a].trig_ms)),
            SortKey::Total => {
                idxs.sort_by(|&a, &b| cmp_f64(ps[b].total_self_ms, ps[a].total_self_ms))
            }
            SortKey::Files => idxs.sort_by(|&a, &b| ps[b].file_count.cmp(&ps[a].file_count)),
            SortKey::Name => idxs.sort_by(|&a, &b| ps[a].name.cmp(&ps[b].name)),
        }
        idxs
    }

    fn selected_plugin_index(&self) -> Option<usize> {
        let vis = self.visible_indices();
        self.table_state
            .selected()
            .and_then(|i| vis.get(i).copied())
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

fn cmp_f64(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
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
    let has_timeline = state.report.phase_timeline.is_some();
    let timeline_h = if has_timeline { 4 } else { 0 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),          // banner
            Constraint::Length(timeline_h), // phase timeline (条件付き)
            Constraint::Min(6),             // plugin table
            Constraint::Length(9),          // detail
            Constraint::Length(3),          // footer
        ])
        .split(area);

    draw_banner(f, chunks[0], state);
    if has_timeline {
        draw_phase_timeline(f, chunks[1], state);
    }
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

    let mut spans = vec![
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
            format!(
                "avg of {} run{}",
                state.report.runs,
                if state.report.runs == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("   "),
        Span::styled(
            truncate(nvim, area.width.saturating_sub(62) as usize),
            Style::default().fg(Color::DarkGray),
        ),
    ];

    if state.report.no_instrument {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            " raw ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ));
    } else if state.report.no_merge {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            " no-merge ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta));
    let widget = Paragraph::new(Line::from(spans))
        .alignment(Alignment::Left)
        .block(block);
    f.render_widget(widget, area);
}

fn draw_phase_timeline(f: &mut Frame, area: Rect, state: &ProfileTuiState) {
    let Some(phases) = state.report.phase_timeline.as_ref() else {
        return;
    };
    let total = phases.iter().map(|p| p.duration_ms).sum::<f64>().max(1e-6);
    let inner_w = area.width.saturating_sub(4) as usize;
    let bar_w = inner_w.saturating_sub(30);

    let lines: Vec<Line> = phases.iter().map(|p| phase_row(p, total, bar_w)).collect();

    let title = Line::from(vec![
        Span::styled(
            " phase timeline ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("(avg per run, ms)", Style::default().fg(Color::DarkGray)),
    ]);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));

    let widget = Paragraph::new(lines).block(block);
    f.render_widget(widget, area);
}

fn phase_row<'a>(p: &'a PhaseTime, total: f64, bar_w: usize) -> Line<'a> {
    let ratio = (p.duration_ms / total).clamp(0.0, 1.0);
    let filled = (ratio * bar_w as f64).round() as usize;
    let color = bar_color(p.duration_ms, total.max(1.0));
    let bar: String = std::iter::repeat_n('\u{2588}', filled)
        .chain(std::iter::repeat_n(
            '\u{2591}',
            bar_w.saturating_sub(filled),
        ))
        .collect();
    let label = phase_label(&p.name);
    Line::from(vec![
        Span::styled(format!(" {:<16}", label), Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{:>7.2} ms  ", p.duration_ms),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(bar, Style::default().fg(color)),
    ])
}

fn phase_label(name: &str) -> String {
    match name {
        "phase-3" => "P3 before".into(),
        "phase-4" => "P4 init".into(),
        "phase-5" => "P5 rtp".into(),
        "phase-6" => "P6 eager".into(),
        "phase-7" => "P7 lazy reg".into(),
        "phase-9" => "P9 after".into(),
        _ => name.to_string(),
    }
}

fn draw_plugin_table(f: &mut Frame, area: Rect, state: &ProfileTuiState) {
    let vis = state.visible_indices();
    // バー幅計算用の最大 (選んでいる sort key に揃える)
    let max_for_sort = vis
        .iter()
        .map(|&i| selector_value(state, i))
        .fold(0.0_f64, f64::max)
        .max(1e-6);

    let inner_w = area.width.saturating_sub(2) as usize;
    // #(3) + kind(6) + plugin(24) + init(8) + load(8) + trig(8) + total(8) + bar
    let bar_w = inner_w.saturating_sub(70).max(6);

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
            "init",
            sort_header_style(state.sort_key == SortKey::Init),
        )),
        Cell::from(Span::styled(
            "load",
            sort_header_style(state.sort_key == SortKey::Load),
        )),
        Cell::from(Span::styled(
            "trig",
            sort_header_style(state.sort_key == SortKey::Trig),
        )),
        Cell::from(Span::styled(
            "total",
            sort_header_style(state.sort_key == SortKey::Total),
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

            let val = selector_value(state, plugin_i);
            let bar_color = bar_color(val, max_for_sort);
            let filled = ((val / max_for_sort) * bar_w as f64).round() as usize;
            let bar: String = std::iter::repeat_n('\u{2588}', filled)
                .chain(std::iter::repeat_n(
                    '\u{2591}',
                    bar_w.saturating_sub(filled),
                ))
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
                    truncate(&p.name, 24),
                    Style::default().fg(name_color),
                )),
                Cell::from(format_ms_cell(p.init_ms, p.is_managed && p.init_ms > 0.0)),
                Cell::from(format_ms_cell(p.load_ms, p.is_managed && !p.lazy)),
                Cell::from(format_ms_cell(p.trig_ms, p.is_managed && p.lazy)),
                Cell::from(Span::styled(
                    format!("{:>6.2}", p.total_self_ms),
                    Style::default().fg(Color::Gray),
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
                "({} shown, sort: {}{})  ",
                vis.len(),
                state.sort_key.label(),
                if state.hide_groups {
                    ", groups hidden"
                } else {
                    ""
                }
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(26),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(7),
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

    let mut ts = state.table_state;
    f.render_stateful_widget(table, area, &mut ts);

    if vis.len() > area.height.saturating_sub(3) as usize {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        let mut sb_state =
            ScrollbarState::new(vis.len()).position(state.table_state.selected().unwrap_or(0));
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

/// 現在のソートキーが参照する値 (バー幅計算 + distribution 色付けに使用)。
fn selector_value(state: &ProfileTuiState, plugin_i: usize) -> f64 {
    let p = &state.report.plugins[plugin_i];
    match state.sort_key {
        SortKey::Load => p.load_ms,
        SortKey::Init => p.init_ms,
        SortKey::Trig => p.trig_ms,
        SortKey::Total => p.total_self_ms,
        SortKey::Files => p.file_count as f64,
        SortKey::Name => p.total_self_ms, // 名前ソート時は total で色分け
    }
}

/// ms 値のセル。対象 phase を持たないプラグイン (lazy の load / eager の trig 等) は `-` 表示。
fn format_ms_cell(value: f64, applicable: bool) -> Span<'static> {
    if !applicable || value <= 0.0 {
        Span::styled("    -   ", Style::default().fg(Color::DarkGray))
    } else {
        Span::styled(
            format!("{:>6.2}  ", value),
            Style::default().fg(Color::Gray),
        )
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
    let header = if p.lazy {
        " trigger registration cost "
    } else if !p.is_managed {
        " sourced files "
    } else {
        " phase 6 files (plugin/, ftdetect/, after/plugin/) "
    };
    lines.push(Line::from(vec![
        Span::styled(header, Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("({} files)", p.file_count),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    if p.top_files.is_empty() {
        if p.lazy {
            lines.push(Line::from(Span::styled(
                " lazy plugin — loaded on trigger, no sourcing during startup",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                " (no sourced files recorded)",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    for (i, file) in p.top_files.iter().take(6).enumerate() {
        let filled = ((file.self_ms / max_file_ms) * bar_w as f64).round() as usize;
        let bar: String = std::iter::repeat_n('\u{2588}', filled)
            .chain(std::iter::repeat_n(
                '\u{2591}',
                bar_w.saturating_sub(filled),
            ))
            .collect();
        let color = bar_color(file.self_ms, max_file_ms);
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:>2}. ", i + 1),
                Style::default().fg(Color::DarkGray),
            ),
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

    let summary = format!(
        "  init {:.2}  load {:.2}  trig {:.2}  /  total {:.2} ms",
        p.init_ms, p.load_ms, p.trig_ms, p.total_self_ms
    );
    let title = Line::from(vec![
        Span::styled(
            format!(" {} ", p.name),
            Style::default()
                .fg(Color::Black)
                .bg(bar_color(
                    p.total_self_ms,
                    state.report.total_startup_ms.max(1.0),
                ))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(summary, Style::default().fg(Color::Gray)),
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
    let widget = Paragraph::new(Line::from(spans))
        .alignment(Alignment::Center)
        .block(
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
    let w = 60.min(area.width.saturating_sub(4));
    let h = 18.min(area.height.saturating_sub(4));
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
        Line::from("  s               cycle sort (load → init → trig → …)"),
        Line::from("  h               toggle [merged]/[runtime] group rows"),
        Line::from("  ?               toggle this help"),
        Line::from("  q / Esc         quit"),
        Line::from(""),
        Line::from(Span::styled(
            "  columns",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  init = phase 4 (per-plugin init.lua, pre-rtp)",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  load = phase 6 (eager plugin/ftdetect/after source)",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  trig = phase 7 (lazy trigger registration)",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  total = sum of all sourced files' self ms",
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// `--no-tui` 用 plain text 出力 (phase timeline + plugin table)。
pub fn print_plain(report: &ProfileReport, top: Option<usize>) {
    println!("# rvpm profile");
    let mode = if report.no_instrument {
        "raw --startuptime"
    } else if report.no_merge {
        "instrumented + no-merge"
    } else {
        "instrumented"
    };
    println!(
        "total_startup_ms = {:.2}   runs = {}   mode = {}   nvim = {}",
        report.total_startup_ms,
        report.runs,
        mode,
        report.nvim_version.as_deref().unwrap_or("(unknown)")
    );

    if let Some(phases) = &report.phase_timeline {
        println!();
        println!("## phase timeline");
        for p in phases {
            println!("  {:<14}  {:>8.2} ms", phase_label(&p.name), p.duration_ms);
        }
    }

    println!();
    println!(
        "  {:>4}  {:>4}  {:<26}  {:>8}  {:>8}  {:>8}  {:>8}  {:>6}",
        "#", "kind", "plugin", "init ms", "load ms", "trig ms", "total", "files"
    );
    println!("  {}", "-".repeat(92));
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
        let show_init = if p.is_managed && p.init_ms > 0.0 {
            format!("{:.2}", p.init_ms)
        } else {
            "-".into()
        };
        let show_load = if p.is_managed && !p.lazy && p.load_ms > 0.0 {
            format!("{:.2}", p.load_ms)
        } else {
            "-".into()
        };
        let show_trig = if p.is_managed && p.lazy && p.trig_ms > 0.0 {
            format!("{:.2}", p.trig_ms)
        } else {
            "-".into()
        };
        println!(
            "  {:>4}  {:>4}  {:<26}  {:>8}  {:>8}  {:>8}  {:>8.2}  {:>6}",
            i + 1,
            kind,
            truncate(&p.name, 26),
            show_init,
            show_load,
            show_trig,
            p.total_self_ms,
            p.file_count,
        );
    }
}
