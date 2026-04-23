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

use crate::profile::{GROUP_USER, PhaseTime, ProfileReport, RequireNode, is_group_name};
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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// 現在どちらの pane が操作対象か。`Tab` で toggle。lazy.nvim の split-view
/// と同じで、focus が当たってる方の枠色を強調し、j/k/g/G を focus pane へ送る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// プラグインテーブル (デフォルト)
    Table,
    /// detail pane — require tree スクロール + expand/collapse
    Detail,
}

impl Focus {
    pub fn toggle(self) -> Self {
        match self {
            Self::Table => Self::Detail,
            Self::Detail => Self::Table,
        }
    }
}

/// `[user config]` の require tree を detail pane に描画するときの並び順。
/// lazy.nvim の Profile view と同じ 2 モード: 時間順 / 登場順。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequireTreeSort {
    /// sourced_ms 降順 (デフォルト) — 重い require を上に寄せる
    ByTime,
    /// init.lua での `require(...)` 呼び出し順 — 依存の流れを追う用
    Chronological,
}

impl RequireTreeSort {
    fn toggle(self) -> Self {
        match self {
            Self::ByTime => Self::Chronological,
            Self::Chronological => Self::ByTime,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ByTime => "by time",
            Self::Chronological => "chrono",
        }
    }
}

/// require tree の threshold cycle: 1.0 → 0.5 → 0.0 → 1.0 (wrap)。
/// 3 段階に絞る理由は lazy.nvim の Profile view と同様、細かい刻みは
/// UX 的に迷うだけで価値が薄いから。
fn next_require_threshold(current: f64) -> f64 {
    if (current - 1.0).abs() < f64::EPSILON {
        0.5
    } else if (current - 0.5).abs() < f64::EPSILON {
        0.0
    } else if current.abs() < f64::EPSILON {
        1.0
    } else {
        // 端数 state からの復帰先 — 一番フィルタ強めで戻す
        1.0
    }
}

/// `flatten_require_tree` が返す 1 行分のデータ。
///
/// `module` は source ツリー (`ProfileReport.plugins[...].require_trace`) の
/// `RequireNode.module` を借用する。row は render-time に毎 frame 作るため、
/// `String::clone` を避けて借用にしたい (lazy.nvim の lifetime-free 構造と違い
/// 我々の RequireNode tree は render loop 中は動かないので borrow 可能)。
///
/// `has_children` / `is_collapsed` は renderer の icon 判定用:
///   - leaf (children 無し): `●`
///   - expanded (children 有り かつ collapsed セット外): `▼`
///   - collapsed (children 有り かつ collapsed セット): `▶`
#[derive(Debug, Clone, PartialEq)]
pub struct RequireRow<'a> {
    pub depth: usize,
    pub module: &'a str,
    pub self_ms: f64,
    pub sourced_ms: f64,
    pub has_children: bool,
    pub is_collapsed: bool,
}

/// RequireNode 木を pre-order DFS で 1 列に flatten する。`threshold_ms` 未満の
/// ノード (およびその子孫) は skip。root だけは必ず残す (pane が空になるのを防ぐ)。
///
/// sort:
///   - ByTime: 兄弟を sourced_ms 降順 (重い require が上)
///   - Chronological: insertion order のまま (init.lua 内の require 呼び出し順)
///
/// max_rows: 可視領域の行数上限。下位の行は切り詰め。
pub fn flatten_require_tree<'a>(
    root: &'a RequireNode,
    threshold_ms: f64,
    sort: RequireTreeSort,
    max_rows: usize,
    collapsed: &std::collections::HashSet<String>,
) -> Vec<RequireRow<'a>> {
    // max_rows は pane の残り行から来る自然な上限 (典型的には 10〜50)。
    // pre-allocate してフレーム毎の re-alloc を回避。
    let mut out = Vec::with_capacity(max_rows);
    walk(
        root,
        0,
        threshold_ms,
        sort,
        max_rows,
        collapsed,
        &mut out,
        true,
    );
    out
}

#[allow(clippy::too_many_arguments)] // 再帰ヘルパなので context struct に切る価値が薄い
fn walk<'a>(
    node: &'a RequireNode,
    depth: usize,
    threshold: f64,
    sort: RequireTreeSort,
    max_rows: usize,
    collapsed: &std::collections::HashSet<String>,
    out: &mut Vec<RequireRow<'a>>,
    is_root: bool,
) {
    if out.len() >= max_rows {
        return;
    }
    // root は threshold で切らない (空 pane 防止)
    if !is_root && node.sourced_ms < threshold {
        return;
    }
    let has_children = !node.children.is_empty();
    let is_collapsed = has_children && collapsed.contains(node.module.as_str());
    out.push(RequireRow {
        depth,
        module: &node.module,
        self_ms: node.self_ms,
        sourced_ms: node.sourced_ms,
        has_children,
        is_collapsed,
    });
    if is_collapsed {
        // 子孫は表示スキップ。node は出すが展開はしない (折りたたみ UX)。
        return;
    }
    // ByTime は並び替えが必要なので一度 Vec<&RequireNode> に集めて sort。
    // Chronological は insertion order のままで良いので直接 iterate して
    // 毎 frame の allocation を避ける。
    match sort {
        RequireTreeSort::ByTime => {
            let mut children: Vec<&RequireNode> = node.children.iter().collect();
            children.sort_by(|a, b| {
                b.sourced_ms
                    .partial_cmp(&a.sourced_ms)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for c in children {
                if out.len() >= max_rows {
                    break;
                }
                walk(
                    c,
                    depth + 1,
                    threshold,
                    sort,
                    max_rows,
                    collapsed,
                    out,
                    false,
                );
            }
        }
        RequireTreeSort::Chronological => {
            for c in &node.children {
                if out.len() >= max_rows {
                    break;
                }
                walk(
                    c,
                    depth + 1,
                    threshold,
                    sort,
                    max_rows,
                    collapsed,
                    out,
                    false,
                );
            }
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
    /// require tree 表示時の sourced_ms 閾値 (ms)。`f` でサイクル。
    require_tree_threshold_ms: f64,
    /// require tree 表示時の兄弟ソート方針。`c` でトグル。
    require_tree_sort: RequireTreeSort,
    /// 操作対象 pane。`Tab` で Table ↔ Detail を切替。
    focus: Focus,
    /// require tree 内のカーソル位置 (flatten 後の行 index)。focus = Detail 時の j/k 対象。
    tree_cursor: usize,
    /// 折りたたまれた require モジュール名。module 名でのみ識別するので、同名モジュールが
    /// 複数カ所で require されているとまとめて折りたたむ扱い (シンプルさ優先)。
    tree_collapsed: std::collections::HashSet<String>,
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
            // 1.0 ms 未満の require は細かすぎてノイズ。まずは bold な top-level だけ
            // 見える状態で start し、f キーで 0.5 / 0.0 ms に広げていく運用。
            require_tree_threshold_ms: 1.0,
            require_tree_sort: RequireTreeSort::ByTime,
            focus: Focus::Table,
            tree_cursor: 0,
            tree_collapsed: std::collections::HashSet::new(),
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

    /// 選択中の [user config] の require tree を flatten して返す (描画 + key
    /// handler で共有)。該当しないプラグインでは None。flatten 結果のサイズは
    /// tree 全体の可視ノード数になるので、cursor clamp もここ基準。
    fn current_require_rows(&self) -> Option<Vec<RequireRow<'_>>> {
        let idx = self.selected_plugin_index()?;
        let p = &self.report.plugins[idx];
        if p.name != GROUP_USER {
            return None;
        }
        let tree = p.require_trace.as_ref()?;
        Some(flatten_require_tree(
            tree,
            self.require_tree_threshold_ms,
            self.require_tree_sort,
            512,
            &self.tree_collapsed,
        ))
    }

    fn tree_cursor_move(&mut self, delta: isize) {
        let Some(rows) = self.current_require_rows() else {
            return;
        };
        let len = rows.len();
        if len == 0 {
            return;
        }
        let cur = self.tree_cursor.min(len - 1) as isize;
        let new = (cur + delta).rem_euclid(len as isize) as usize;
        self.tree_cursor = new;
    }

    /// `G` / End 相当: cursor を tree の末尾行にセット。`usize::MAX` の sentinel
    /// 方式より、実際の行数を解決してから入れる方が state の不変条件が崩れにくい。
    fn tree_go_bottom(&mut self) {
        let Some(rows) = self.current_require_rows() else {
            return;
        };
        if !rows.is_empty() {
            self.tree_cursor = rows.len() - 1;
        }
    }

    /// `h` (collapse=true) / `l` (collapse=false) の共通処理。
    /// cursor が指すノードの module 名を tree_collapsed セットに追加/削除する。
    /// 同名モジュールを別経路で require している場合はまとめて影響する (意図的な単純化)。
    fn tree_toggle_at_cursor(&mut self, collapse: bool) {
        let Some(rows) = self.current_require_rows() else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let cur = self.tree_cursor.min(rows.len() - 1);
        let row = &rows[cur];
        if !row.has_children {
            return;
        }
        let name = row.module.to_string();
        if collapse {
            self.tree_collapsed.insert(name);
        } else {
            self.tree_collapsed.remove(&name);
        }
    }
}

fn cmp_f64(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

/// raw mode + alt screen を借りている間、panic / 早期 return でも必ず戻す RAII guard。
///
/// `enable_raw_mode()` 直後に `EnterAlternateScreen` や `Terminal::new` が失敗すると、
/// cleanup ルートが走らず端末が壊れた状態で返ってしまう。`Drop` 実装で明示的に
/// 後始末を走らせることで、どのルートから抜けても端末を戻せるようにする。
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
        // カーソル表示は crossterm の show_cursor() を直接叩く
        let _ = execute!(stdout, crossterm::cursor::Show);
    }
}

/// エントリポイント: TUI を起動してユーザが q で終了するまでブロック。
pub fn run(report: ProfileReport) -> Result<()> {
    enable_raw_mode()?;
    // raw mode を掴んだ直後に guard を作る — 以降の `?` や panic は Drop 経由で cleanup される。
    let _guard = TerminalGuard;

    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let mut state = ProfileTuiState::new(report);
    let result = run_loop(&mut terminal, &mut state);

    // 正常終了ルート (Drop も追加で走るが冪等)
    terminal.show_cursor()?;
    result
}

fn run_loop(
    terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &mut ProfileTuiState,
) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, state))?; // state は &mut で渡す (ratatui の state mutation を失わないため)
        if event::poll(std::time::Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                // Tab で pane focus を切り替え。Detail focus は [user config] を選んで
                // require_trace がある時だけ意味を持つが、key 自体はいつでも受け付ける
                // (focus 状態を保ったまま別 plugin を選んでも混乱しないように)。
                KeyCode::Tab => state.focus = state.focus.toggle(),
                // focus 別に j/k/g/G を分岐。Detail 時は tree の cursor を動かし、
                // Table 時は従来通り plugin 行を選ぶ。
                KeyCode::Char('j') | KeyCode::Down => match state.focus {
                    Focus::Table => state.move_by(1),
                    Focus::Detail => state.tree_cursor_move(1),
                },
                KeyCode::Char('k') | KeyCode::Up => match state.focus {
                    Focus::Table => state.move_by(-1),
                    Focus::Detail => state.tree_cursor_move(-1),
                },
                KeyCode::Char('g') | KeyCode::Home => match state.focus {
                    Focus::Table => state.go_top(),
                    Focus::Detail => state.tree_cursor = 0,
                },
                KeyCode::Char('G') | KeyCode::End => match state.focus {
                    Focus::Table => state.go_bottom(),
                    Focus::Detail => state.tree_go_bottom(),
                },
                // lazy.nvim / nvim-tree 系の操作感: h は collapse、l は expand。
                // Detail focus 時のみ有効。Table focus 時は no-op (誤爆防止)。
                KeyCode::Char('l') | KeyCode::Right if state.focus == Focus::Detail => {
                    state.tree_toggle_at_cursor(false);
                }
                KeyCode::Char('h') | KeyCode::Left if state.focus == Focus::Detail => {
                    state.tree_toggle_at_cursor(true);
                }
                KeyCode::Char('s') => state.sort_key = state.sort_key.next(),
                KeyCode::Char('h') => state.hide_groups = !state.hide_groups,
                KeyCode::Char('?') => state.show_help = !state.show_help,
                // require tree detail pane 専用キー ([user config] 選択中のみ意味あり)。
                // 他のプラグイン選択時は値が変わるだけで描画に影響しないので安全。
                KeyCode::Char('f') => {
                    state.require_tree_threshold_ms =
                        next_require_threshold(state.require_tree_threshold_ms);
                }
                KeyCode::Char('c') => {
                    state.require_tree_sort = state.require_tree_sort.toggle();
                }
                _ => {}
            }
        }
    }
}

fn draw(f: &mut Frame, state: &mut ProfileTuiState) {
    let area = f.area();
    let has_timeline = state.report.phase_timeline.is_some();
    // 7 phases (3/4/5/6/7/8/9) × 1 row + 2 rows for the rounded block border = 9。
    let timeline_h = if has_timeline { 9 } else { 0 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),          // banner
            Constraint::Length(timeline_h), // phase timeline (条件付き)
            Constraint::Min(6),             // plugin table
            Constraint::Length(14),         // detail — 9→14 で require tree を広く見せる
            Constraint::Length(3),          // footer
        ])
        .split(area);

    draw_banner(f, chunks[0], state);
    if has_timeline {
        draw_phase_timeline(f, chunks[1], state);
    }
    // plugin table のみ state を mutate する (ratatui の render_stateful_widget が
    // TableState.offset を更新するため)。TableState は Copy 実装なので単純 copy
    // すると描画後の offset 更新が破棄され、大量の plugin がある時にスクロール
    // 位置が伝わらない。
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
        "phase-8" => "P8 colorscheme".into(),
        "phase-9" => "P9 after".into(),
        _ => name.to_string(),
    }
}

fn draw_plugin_table(f: &mut Frame, area: Rect, state: &mut ProfileTuiState) {
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

    // `&mut state.table_state` を直接渡す — TableState は Copy だが、copy を渡すと
    // render_stateful_widget による offset 更新が捨てられて大量 plugin のスクロール
    // が機能しない。
    f.render_stateful_widget(table, area, &mut state.table_state);

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

/// [user config] 選択時の detail pane — init.lua からの require tree を
/// 木構造で縦一列に並べる。`f` で threshold (1.0/0.5/0.0 ms) サイクル、
/// `c` で sort (ByTime ↔ Chronological) トグル。lazy.nvim の Profile
/// view と同じ操作感。
fn draw_require_tree_detail(
    f: &mut Frame,
    area: Rect,
    state: &ProfileTuiState,
    plugin: &crate::profile::PluginStats,
    tree: &RequireNode,
) {
    // pane 内部で使える行数 (banner 1 行 + border 2 行を引く)。
    // banner が 1 行占めるので tree 描画行数は inner_h - 1。
    let inner_h = area.height.saturating_sub(3) as usize;
    let body_h = inner_h.saturating_sub(1).max(1);
    // flatten 時点では全 tree を取り、後段の描画ループで cursor 位置を中心に
    // スクロールさせる。上限 512 はどんなに require 連鎖が深くても処理を
    // 終わらせるための cap。
    let rows = flatten_require_tree(
        tree,
        state.require_tree_threshold_ms,
        state.require_tree_sort,
        512,
        &state.tree_collapsed,
    );

    // bar は sourced_ms 基準 (「全体の実時間にどれだけ食われてるか」を見るため)。
    // rows[0] = root = sourced_ms の最大 (parent >= descendants 不変量)。
    let max_sourced = rows.first().map(|r| r.sourced_ms).unwrap_or(0.0).max(1e-6);
    let bar_w = (area.width.saturating_sub(60) as usize).max(4);

    // cursor を可視範囲に保つようスクロール offset を計算。cursor を window
    // 中央付近に置き、両端では clamp。
    let cursor = state.tree_cursor.min(rows.len().saturating_sub(1));
    let scroll = if rows.len() <= body_h {
        0
    } else {
        let half = body_h / 2;
        cursor.saturating_sub(half).min(rows.len() - body_h)
    };
    let visible_end = (scroll + body_h).min(rows.len());
    let focused = state.focus == Focus::Detail;

    let mut lines: Vec<Line> = Vec::with_capacity(body_h + 1);
    let total_nodes = count_require_nodes(tree);
    lines.push(Line::from(vec![
        Span::styled(" require tree ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(
                "({}-{} of {} · threshold {:.1} ms · sort {})",
                scroll + 1,
                visible_end,
                total_nodes,
                state.require_tree_threshold_ms,
                state.require_tree_sort.label(),
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    for (idx, row) in rows[scroll..visible_end].iter().enumerate() {
        let row_idx = scroll + idx;
        let is_cursor = focused && row_idx == cursor;
        let icon = if row.has_children {
            if row.is_collapsed {
                '\u{25b6}'
            } else {
                '\u{25bc}'
            } // ▶ / ▼
        } else {
            require_tree_icon(row.depth) // ●/○/◉
        };
        let indent: String = "  ".repeat(row.depth);
        let filled = ((row.sourced_ms / max_sourced) * bar_w as f64).round() as usize;
        let bar: String = std::iter::repeat_n('\u{2588}', filled)
            .chain(std::iter::repeat_n(
                '\u{2591}',
                bar_w.saturating_sub(filled),
            ))
            .collect();
        let color = bar_color(row.sourced_ms, max_sourced);
        let name_width = 34usize.saturating_sub(row.depth * 2).max(8);
        let cursor_marker = if is_cursor { '\u{25b6}' } else { ' ' }; // ▶ selection
        let name_style = if is_cursor {
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}{}{} ", cursor_marker, indent, icon),
                Style::default().fg(if is_cursor {
                    Color::Magenta
                } else {
                    Color::DarkGray
                }),
            ),
            Span::styled(pad_truncate(row.module, name_width), name_style),
            Span::styled(
                format!("  {:>6.2} ms  ", row.sourced_ms),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(bar, Style::default().fg(color)),
        ]));
    }

    let summary = format!(
        "  self {:.2}  sourced {:.2}  /  total {:.2} ms",
        tree.self_ms, tree.sourced_ms, plugin.total_self_ms
    );
    let title = Line::from(vec![
        Span::styled(
            format!(" {} ", plugin.name),
            Style::default()
                .fg(Color::Black)
                .bg(bar_color(
                    plugin.total_self_ms,
                    state.report.total_startup_ms.max(1.0),
                ))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(summary, Style::default().fg(Color::Gray)),
    ]);
    // focus が当たってる時は枠を Magenta に (どちらの pane を操作してるか一目瞭然)。
    let border_color = if focused {
        Color::Magenta
    } else {
        Color::DarkGray
    };
    let widget = Paragraph::new(lines).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color)),
    );
    f.render_widget(widget, area);
}

fn require_tree_icon(depth: usize) -> char {
    // lazy.nvim の list icon と同じ雰囲気: 深さに応じて黒丸 → 白丸 → 二重丸 と
    // 階層がつくループ。
    match depth % 3 {
        0 => '\u{25cf}', // ●
        1 => '\u{25cb}', // ○
        _ => '\u{25c9}', // ◉
    }
}

fn count_require_nodes(node: &RequireNode) -> usize {
    1 + node.children.iter().map(count_require_nodes).sum::<usize>()
}

fn draw_detail(f: &mut Frame, area: Rect, state: &ProfileTuiState) {
    let Some(idx) = state.selected_plugin_index() else {
        return;
    };
    let p = &state.report.plugins[idx];

    // `[user config]` で require_trace が populate されているなら、top_files の
    // 代わりに require tree を描画する (#77 PR 3)。
    if p.name == GROUP_USER
        && let Some(tree) = p.require_trace.as_ref()
    {
        draw_require_tree_detail(f, area, state, p, tree);
        return;
    }

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
                pad_truncate(&file.relative_path, 34),
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
        ("Tab", "focus"),
        ("h/l", "collapse"),
        ("s/c/f", "sort/thresh"),
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
    // 元は 60x18 だったが require tree 用のキー説明が右端に溢れるので 76x24 に拡張。
    let w = 76.min(area.width.saturating_sub(4));
    let h = 24.min(area.height.saturating_sub(4));
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
        Line::from(" Tab              toggle focus (plugin table ↔ require tree)"),
        Line::from(""),
        Line::from(Span::styled(
            "  plugin table (default focus)",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  h               toggle [merged]/[runtime] group rows"),
        Line::from(""),
        Line::from(Span::styled(
            "  require tree ([user config], focus = Detail)",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  h / ←           collapse subtree at cursor"),
        Line::from("  l / →           expand subtree at cursor"),
        Line::from("  f               threshold cycle (1.0 → 0.5 → 0.0 ms)"),
        Line::from("  c               sort toggle (by time ↔ chronological)"),
        Line::from(""),
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

/// 表示幅 (`unicode-width`) が `max` を超える場合に末尾を U+2026 (`…`) に置き換えて
/// 切り詰める。全角文字を含む文字列でも ms 値やバーの開始列が揃うよう、codepoint
/// 数ではなくターミナル上の表示幅で判定する。`browse_tui.rs` の列幅計算と同じ方針。
fn truncate(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('\u{2026}');
    out
}

/// `truncate` で上限表示幅に揃えた上で、短ければ末尾をスペースでパディングして
/// **必ず表示幅 `width` ぴったり**にする。Paragraph 内で後続の Span (ms 値やバー)
/// の開始列を揃えるのに使う (Table の Cell と違って Paragraph は自動パディング
/// しない)。`format!("{:<width$}")` は codepoint 数でパディングするので全角文字
/// だと幅が合わず、unicode-width ベースに揃える必要がある。
fn pad_truncate(s: &str, width: usize) -> String {
    let truncated = truncate(s, width);
    let w = UnicodeWidthStr::width(truncated.as_str());
    if w < width {
        let mut out = truncated;
        out.extend(std::iter::repeat_n(' ', width - w));
        out
    } else {
        truncated
    }
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

    // [user config] に require_trace があれば require tree を追加出力する。
    // plain text 出力は pipe / grep 友好的なので threshold は設けず全ノード
    // 出す方針 (TUI と違って隠す動機が薄い)。sort は TUI のデフォルトと同じ
    // ByTime で、自動化用途 (ログ収集) でも読みやすい順序になる。
    if let Some(user_cfg) = report
        .plugins
        .iter()
        .find(|p| p.name == crate::profile::GROUP_USER)
        && let Some(tree) = user_cfg.require_trace.as_ref()
    {
        println!();
        let nodes = count_require_nodes(tree);
        println!(
            "## require tree ({} nodes, sourced {:.2} ms, self {:.2} ms)",
            nodes, tree.sourced_ms, tree.self_ms
        );
        print_require_tree_plain(tree, 0);
    }
}

fn print_require_tree_plain(node: &RequireNode, depth: usize) {
    let indent: String = "  ".repeat(depth);
    println!(
        "  {}{:<40}  sourced {:>7.2} ms · self {:>7.2} ms",
        indent, node.module, node.sourced_ms, node.self_ms,
    );
    let mut children: Vec<&RequireNode> = node.children.iter().collect();
    children.sort_by(|a, b| {
        b.sourced_ms
            .partial_cmp(&a.sourced_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for c in children {
        print_require_tree_plain(c, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::RequireNode;
    use unicode_width::UnicodeWidthStr;

    // ── require-tree flattening for detail pane ────────────────────────────
    // [user config] を選んだとき、require_trace を行単位に flatten して
    // detail pane に縦一列で並べる。引数で threshold / sort / max_rows を受け、
    // 描画ループに渡す行を決める。
    //
    // * `threshold_ms`: sourced_ms がこれ未満のノードとその子孫をカット。
    //   1.0 / 0.5 / 0.0 の 3 段階を `f` キーでサイクル。
    // * `sort`: 兄弟をどの順で並べるか。ByTime (sourced_ms desc) が default。
    //   `c` で Chronological (insertion order) に切り替え、lazy.nvim の
    //   Profile view と同じ 2 モード。
    // * `max_rows`: pane の残り行数を超えたら切り詰める。

    fn sample_tree() -> RequireNode {
        // init.lua 10ms, children: [A 7ms (A1 5ms, A2 1ms), B 0.4ms, C 2ms]
        // threshold=1.0 で B (0.4ms) 全体カット、A2 (1ms, sourced) はボーダー。
        RequireNode {
            module: "init.lua".into(),
            self_ms: 0.6,
            sourced_ms: 10.0,
            children: vec![
                RequireNode {
                    module: "A".into(),
                    self_ms: 1.0,
                    sourced_ms: 7.0,
                    children: vec![
                        RequireNode {
                            module: "A1".into(),
                            self_ms: 5.0,
                            sourced_ms: 5.0,
                            children: vec![],
                        },
                        RequireNode {
                            module: "A2".into(),
                            self_ms: 1.0,
                            sourced_ms: 1.0,
                            children: vec![],
                        },
                    ],
                },
                RequireNode {
                    module: "B".into(),
                    self_ms: 0.4,
                    sourced_ms: 0.4,
                    children: vec![],
                },
                RequireNode {
                    module: "C".into(),
                    self_ms: 2.0,
                    sourced_ms: 2.0,
                    children: vec![],
                },
            ],
        }
    }

    #[test]
    fn flatten_require_tree_includes_root_with_depth_0() {
        let tree = sample_tree();
        let rows = flatten_require_tree(
            &tree,
            0.0,
            RequireTreeSort::ByTime,
            99,
            &std::collections::HashSet::new(),
        );
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[0].module, "init.lua");
    }

    #[test]
    fn flatten_require_tree_sorts_siblings_by_sourced_ms_desc() {
        let tree = sample_tree();
        let rows = flatten_require_tree(
            &tree,
            0.0,
            RequireTreeSort::ByTime,
            99,
            &std::collections::HashSet::new(),
        );
        // init.lua → A (7) → A1 (5) → A2 (1) → C (2) → B (0.4)
        let modules: Vec<&str> = rows.iter().map(|r| r.module).collect();
        assert_eq!(modules, vec!["init.lua", "A", "A1", "A2", "C", "B"]);
    }

    #[test]
    fn flatten_require_tree_keeps_insertion_order_under_chronological() {
        let tree = sample_tree();
        let rows = flatten_require_tree(
            &tree,
            0.0,
            RequireTreeSort::Chronological,
            99,
            &std::collections::HashSet::new(),
        );
        // init.lua → A → A1 → A2 → B → C (Chronological では source 順)
        let modules: Vec<&str> = rows.iter().map(|r| r.module).collect();
        assert_eq!(modules, vec!["init.lua", "A", "A1", "A2", "B", "C"]);
    }

    #[test]
    fn flatten_require_tree_skips_subtrees_below_threshold() {
        let tree = sample_tree();
        // threshold=1.0 は sourced_ms < 1.0 をカット。B (0.4) は消える。
        // A2 (1.0) はちょうど閾値なので残る (< ではなく >= で判定)。
        let rows = flatten_require_tree(
            &tree,
            1.0,
            RequireTreeSort::ByTime,
            99,
            &std::collections::HashSet::new(),
        );
        let modules: Vec<&str> = rows.iter().map(|r| r.module).collect();
        assert!(!modules.contains(&"B"));
        assert!(modules.contains(&"A2")); // 境界値は残す
    }

    #[test]
    fn flatten_require_tree_never_cuts_root_even_below_threshold() {
        // root (init.lua) の sourced_ms が threshold 未満でも、少なくとも root は
        // 表示する。そうしないと detail pane が空になって "何が選ばれてるのか"
        // 分からなくなる。
        let tiny = RequireNode {
            module: "init.lua".into(),
            self_ms: 0.1,
            sourced_ms: 0.1,
            children: vec![],
        };
        let rows = flatten_require_tree(
            &tiny,
            1.0,
            RequireTreeSort::ByTime,
            99,
            &std::collections::HashSet::new(),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].module, "init.lua");
    }

    #[test]
    fn flatten_require_tree_respects_max_rows() {
        let tree = sample_tree();
        let rows = flatten_require_tree(
            &tree,
            0.0,
            RequireTreeSort::ByTime,
            3,
            &std::collections::HashSet::new(),
        );
        assert_eq!(rows.len(), 3);
        // 早い順に 3 行: init.lua / A / A1
        assert_eq!(rows[0].module, "init.lua");
        assert_eq!(rows[1].module, "A");
        assert_eq!(rows[2].module, "A1");
    }

    #[test]
    fn flatten_require_tree_carries_sourced_and_self_ms_and_depth() {
        // bar 描画側が使うフィールド (self_ms / sourced_ms / depth) が正しく
        // 渡されること。色分け用に self と sourced の両方が必要。
        let tree = sample_tree();
        let rows = flatten_require_tree(
            &tree,
            0.0,
            RequireTreeSort::ByTime,
            99,
            &std::collections::HashSet::new(),
        );
        let a = rows.iter().find(|r| r.module == "A").unwrap();
        assert_eq!(a.depth, 1);
        assert!((a.sourced_ms - 7.0).abs() < 1e-9);
        assert!((a.self_ms - 1.0).abs() < 1e-9);
        let a1 = rows.iter().find(|r| r.module == "A1").unwrap();
        assert_eq!(a1.depth, 2);
    }

    #[test]
    fn flatten_require_tree_marks_has_children_for_renderer() {
        // renderer が `▶` (collapsed) / `▼` (expanded) / `●` (leaf) を出し分けるため、
        // 行に has_children フラグが必要。
        let tree = sample_tree();
        let rows = flatten_require_tree(
            &tree,
            0.0,
            RequireTreeSort::ByTime,
            99,
            &std::collections::HashSet::new(),
        );
        let root = &rows[0];
        assert!(root.has_children, "init.lua は 3 children あり");
        assert!(!root.is_collapsed);
        let b = rows.iter().find(|r| r.module == "B").unwrap();
        assert!(!b.has_children, "B は leaf");
        let a1 = rows.iter().find(|r| r.module == "A1").unwrap();
        assert!(!a1.has_children, "A1 も leaf");
    }

    #[test]
    fn flatten_require_tree_hides_children_of_collapsed_nodes() {
        // collapsed set に "A" があれば、A は出すが A1 / A2 は出さない。
        let tree = sample_tree();
        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert("A".to_string());
        let rows = flatten_require_tree(&tree, 0.0, RequireTreeSort::ByTime, 99, &collapsed);
        let modules: Vec<&str> = rows.iter().map(|r| r.module).collect();
        assert!(modules.contains(&"A"));
        assert!(!modules.contains(&"A1"));
        assert!(!modules.contains(&"A2"));
        // A は collapsed 状態で、has_children も true のまま
        let a = rows.iter().find(|r| r.module == "A").unwrap();
        assert!(a.has_children);
        assert!(a.is_collapsed);
    }

    // ── focus + tree cursor ────────────────────────────────────────────────
    // Tab で pane 間 focus を切り替え、detail pane focus 時に tree の行を選べる。
    // focus = Table → j/k はテーブル選択、focus = Detail → j/k は tree 行選択。

    #[test]
    fn focus_toggles_between_table_and_detail() {
        assert_eq!(Focus::Table.toggle(), Focus::Detail);
        assert_eq!(Focus::Detail.toggle(), Focus::Table);
    }

    // ── require-tree state keybindings ──────────────────────────────────────

    #[test]
    fn require_tree_threshold_cycles_through_three_steps() {
        // f キー用: 1.0 → 0.5 → 0.0 → 1.0 (wrap)。3 段階に絞る理由は
        // lazy.nvim の Profile view が同様の離散ステップで、細かすぎる
        // 刻みは UX 的に迷うだけのため。
        assert_eq!(next_require_threshold(1.0), 0.5);
        assert_eq!(next_require_threshold(0.5), 0.0);
        assert_eq!(next_require_threshold(0.0), 1.0);
        // 端数は次の刻みに丸められる (壊れた state からの復帰)
        assert_eq!(next_require_threshold(0.123), 1.0);
    }

    #[test]
    fn require_tree_sort_toggles_between_two_modes() {
        assert_eq!(
            RequireTreeSort::ByTime.toggle(),
            RequireTreeSort::Chronological
        );
        assert_eq!(
            RequireTreeSort::Chronological.toggle(),
            RequireTreeSort::ByTime
        );
    }

    #[test]
    fn pad_truncate_pads_short_strings_to_exact_display_width() {
        // 短い文字列はスペースパディングで幅ぴったり。detail panel のバー開始列を
        // 揃えるのに必須 (短いパスでも ms 値 / バーが同じ列から始まらないと
        // 行ごとにズレて見える)。
        let out = pad_truncate("plugin/denops.vim", 34);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 34);
        assert!(out.starts_with("plugin/denops.vim"));
        assert!(out.ends_with(' '));
    }

    #[test]
    fn pad_truncate_truncates_long_strings_to_exact_display_width() {
        let long = "autoload/denops/_internal/very/deep/nested/file.vim";
        let out = pad_truncate(long, 34);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 34);
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn pad_truncate_preserves_exact_width_input() {
        let exact: String = "x".repeat(34);
        let out = pad_truncate(&exact, 34);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 34);
        assert_eq!(out, exact);
    }

    #[test]
    fn pad_truncate_measures_full_width_chars_by_display_width() {
        // 全角文字は 1 codepoint = 2 display columns。chars().count() ベースだと
        // パディング幅を誤って出力列がズレる (例: 全角 10 文字を width=20 だと
        // 本来ピッタリなのに char count=10 で更に 10 spaces パディングしてしまう)。
        let jp = "日本語プラグイン.vim"; // 全角 9 文字 + ".vim" = 18 + 4 = 22 columns
        let out = pad_truncate(jp, 34);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 34);
        assert!(out.starts_with(jp));
    }

    #[test]
    fn truncate_respects_display_width_when_clipping_full_width_chars() {
        // 全角文字 3 個 = 6 columns を width=5 に切り詰めると、収まるのは
        // 2 文字 (4 columns) + ellipsis (1 column) = 5 columns。
        let out = truncate("日本語", 5);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 5);
        assert!(out.ends_with('\u{2026}'));
    }
}
