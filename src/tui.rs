use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Gauge, List, ListItem, ListState, Paragraph, Row,
        Table, TableState,
    },
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
    /// clone は健全だが「前回の `rvpm update` が失敗した」状態
    /// (#update-error-visibility)。`rvpm list` が update_errors.json から
    /// overlay して表示する専用マーカー。git status 上は Clean なので
    /// Failed とは別扱いにして、赤 [Error] と混同させない。
    UpdateFailed(String),
}

/// `init.lua` / `before.lua` / `after.lua` の存在フラグ。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HookFlags {
    pub init: bool,
    pub before: bool,
    pub after: bool,
}

impl HookFlags {
    fn any(&self) -> bool {
        self.init || self.before || self.after
    }

    /// `I B A` 列の表示テキストと色。
    fn render(&self, icons: &Icons, theme: &crate::theme::Theme) -> (String, Color) {
        let mark = |on: bool| if on { icons.hook_on } else { icons.hook_off };
        (
            format!(
                "{} {} {}",
                mark(self.init),
                mark(self.before),
                mark(self.after)
            ),
            if self.any() {
                theme.success
            } else {
                theme.muted
            },
        )
    }
}

/// `draw_list` が使う hook 存在フラグのスナップショット。
///
/// `exists()` は Windows の NTFS + リアルタイムスキャン下では 1 回あたり
/// 0.1ms 規模かかる。252 plugin × 3 file を毎フレーム叩くと 1 描画で 150ms
/// 近くになり、j/k のカーソル移動が体感で詰まる (#list-tui-latency)。
/// config を読んだタイミングで 1 度だけ走査して、描画側は参照するだけにする。
pub struct HookCache {
    /// `[ Global hooks ]` sentinel 行の分。`None` なら sentinel を描かない。
    pub global: Option<HookFlags>,
    /// `config.plugins` と同じ並び / 同じ長さ。
    pub plugins: Vec<HookFlags>,
}

impl HookCache {
    /// `config_root` 配下の per-plugin hook と、global hook の存在を走査する。
    /// `nvim_init_lua` に `Some` を渡したときだけ sentinel 行の分を作る。
    pub fn scan(
        config: &crate::config::Config,
        config_root: &std::path::Path,
        nvim_init_lua: Option<&std::path::Path>,
    ) -> Self {
        let global = nvim_init_lua.map(|init_lua| HookFlags {
            init: init_lua.exists(),
            before: config_root.join("before.lua").exists(),
            after: config_root.join("after.lua").exists(),
        });
        let plugins = config
            .plugins
            .iter()
            .map(|p| {
                let dir = crate::paths::resolve_plugin_config_dir(config_root, p);
                HookFlags {
                    init: dir.join("init.lua").exists(),
                    before: dir.join("before.lua").exists(),
                    after: dir.join("after.lua").exists(),
                }
            })
            .collect();
        Self { global, plugins }
    }
}

/// `rvpm list` の theme picker (`T` キー) が保持する選択状態。
/// `presets` は `Config::options.theme_preset_list()` の結果 (組み込み +
/// custom をマージ済み、アルファベット順) をそのまま持つ。
pub struct ThemePickerState {
    /// `(name, theme, is_custom)`。
    pub presets: Vec<(String, crate::theme::Theme, bool)>,
    pub selected: usize,
    /// Enter が 1 度押されて確認待ちになっている状態。preset 適用は
    /// `[options.theme]` の 14 field を丸ごと捨てる破壊的な書き込みで、
    /// 手書きした色を戻す手段がアプリ内に無い (しかも初回は `theme_preset`
    /// 未設定なので選択行は一覧の先頭 = ユーザーの現在の色とは無関係)。
    /// `run_remove` が `dialoguer::Confirm` を挟むのと同じ理由で、実際に
    /// 書き込むのは確認待ち中の 2 度目の Enter (または `y`) だけにする。
    pub confirming: bool,
}

impl ThemePickerState {
    /// `active_name` が `presets` に見つかればその行を初期選択にする
    /// (前回 picker で適用した preset を再度開いたときにハイライトされる)。
    /// 見つからなければ先頭 (0) を選択。
    pub fn new(
        presets: Vec<(String, crate::theme::Theme, bool)>,
        active_name: Option<&str>,
    ) -> Self {
        let selected = active_name
            .and_then(|name| presets.iter().position(|(n, _, _)| n == name))
            .unwrap_or(0);
        Self {
            presets,
            selected,
            confirming: false,
        }
    }

    /// 選択を動かしたら確認待ちは必ず解除する (確認プロンプトに出ている
    /// preset 名と、実際に書き込まれる preset がズレないように)。
    pub fn next(&mut self) {
        self.confirming = false;
        if !self.presets.is_empty() {
            self.selected = (self.selected + 1) % self.presets.len();
        }
    }

    pub fn previous(&mut self) {
        self.confirming = false;
        if !self.presets.is_empty() {
            self.selected = (self.selected + self.presets.len() - 1) % self.presets.len();
        }
    }

    /// Enter 1 度目: 選択中 preset の適用を確認待ちにする。選択できる行が
    /// 無ければ (preset 0 件) 確認するものも無いので何もしない。
    pub fn request_confirm(&mut self) {
        if !self.presets.is_empty() {
            self.confirming = true;
        }
    }

    /// 確認待ちを取り消す (picker 自体は開いたまま)。
    pub fn cancel_confirm(&mut self) {
        self.confirming = false;
    }

    pub fn current(&self) -> Option<&(String, crate::theme::Theme, bool)> {
        self.presets.get(self.selected)
    }
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
    /// `T` キーで開く theme picker のモーダル状態。`Some` の間は j/k で
    /// プリセットをライブプレビューしながら選び、Enter で確定・config.toml へ
    /// 永続化、Esc でキャンセル (config は無変更)。
    pub theme_picker: Option<ThemePickerState>,
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
            theme_picker: None,
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
    fn progress_color(ratio: f64, theme: &crate::theme::Theme) -> Color {
        if ratio < 0.25 {
            theme.error
        } else if ratio < 0.5 {
            theme.warning
        } else if ratio < 0.75 {
            theme.info
        } else {
            theme.success
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

    pub fn draw(
        &mut self,
        f: &mut Frame,
        message: &str,
        icons: &Icons,
        theme: &crate::theme::Theme,
    ) {
        f.render_widget(Block::default().style(theme.base_style()), f.area());
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
        let gauge_color = Self::progress_color(ratio, theme);

        // "syncing..." を dots animation 付きで、末尾に mm:ss と N in flight を足す。
        let message_trim = message.trim_end_matches(['.', ' ']);
        let animated_msg = format!("{}{}", message_trim, self.dots_frame());

        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                " rvpm ",
                Style::default()
                    .fg(theme.inverse)
                    .bg(gauge_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}  ", animated_msg),
                Style::default().fg(theme.secondary),
            ),
            Span::styled(
                format!("{}", finished_count),
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("/", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{}", self.plugins.len()),
                Style::default().fg(theme.foreground),
            ),
            if syncing_count > 0 {
                Span::styled(
                    format!("  {}{} ", self.spinner_frame(icons.style), syncing_count),
                    Style::default().fg(theme.info),
                )
            } else {
                Span::raw("  ")
            },
            if failed_count > 0 {
                Span::styled(
                    format!(" {}err", failed_count),
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("")
            },
            Span::styled("   ", Style::default()),
            Span::styled(
                format!("⏱ {}", self.elapsed_str()),
                Style::default().fg(theme.muted),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.muted)),
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
                        theme.muted,
                        Style::default().fg(theme.muted),
                        "Waiting…".to_string(),
                        theme.muted,
                    ),
                    PluginStatus::Syncing(m) => (
                        spinner_char,
                        theme.info,
                        Style::default().fg(theme.info).add_modifier(Modifier::BOLD),
                        m.clone(),
                        theme.info,
                    ),
                    PluginStatus::Finished => (
                        icons.finished,
                        theme.success,
                        Style::default().fg(theme.secondary),
                        "Finished".to_string(),
                        theme.muted,
                    ),
                    PluginStatus::Failed(e) => (
                        icons.failed,
                        theme.error,
                        Style::default()
                            .fg(theme.error)
                            .add_modifier(Modifier::BOLD),
                        e.clone(),
                        theme.error,
                    ),
                    // 進捗 TUI (sync/update 実行中) では発生しないが exhaustive
                    // match のため。万一渡っても update 失敗として黄色で見せる。
                    PluginStatus::UpdateFailed(e) => (
                        icons.failed,
                        theme.warning,
                        Style::default()
                            .fg(theme.warning)
                            .add_modifier(Modifier::BOLD),
                        e.clone(),
                        theme.warning,
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
            theme.error
        } else if syncing_count > 0 {
            gauge_color
        } else if done_count == self.plugins.len() && !self.plugins.is_empty() {
            theme.success
        } else {
            theme.muted
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
                            .fg(theme.foreground)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("({} in flight) ", syncing_count),
                        Style::default().fg(theme.muted),
                    ),
                ]))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(table_border_color)),
        )
        .row_highlight_style(
            Style::default()
                .bg(theme.selection_background)
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
                    .fg(theme.inverse)
                    .add_modifier(Modifier::BOLD),
            ))
            .ratio(ratio.clamp(0.0, 1.0));
        f.render_widget(gauge, chunks[2]);
    }

    pub fn draw_list(
        &mut self,
        f: &mut Frame,
        config: &crate::config::Config,
        icons: &Icons,
        // hook 存在フラグは `HookCache::scan` で config 読み込み時に 1 度だけ
        // 走査したものを受け取る。`hooks.global` が `Some` のときだけ一番上に
        // `[ Global hooks ]` sentinel 行を描く (その場合 `tui_state.plugins[0]`
        // は空文字 sentinel で、`selected_url()` が `Some("")` を返す前提)。
        hooks: &HookCache,
        theme: &crate::theme::Theme,
    ) {
        // theme picker が開いている間は、選択中の行の色でフレーム全体を
        // ライブプレビューする (Enter で確定するまで config.toml も
        // `config.options.theme` も一切変更しない)。
        let picker_preview_theme;
        let theme: &crate::theme::Theme = match &self.theme_picker {
            Some(picker) => {
                picker_preview_theme = picker.current().map(|(_, t, _)| *t).unwrap_or(*theme);
                &picker_preview_theme
            }
            None => theme,
        };
        f.render_widget(Block::default().style(theme.base_style()), f.area());
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
                    .fg(theme.inverse)
                    .bg(theme.info)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}:", config.plugins.len()),
                Style::default().fg(theme.foreground),
            ),
            Span::styled("total ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{}:", eager_count),
                Style::default().fg(theme.success),
            ),
            Span::styled("eager ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{}:", lazy_count),
                Style::default().fg(theme.warning),
            ),
            Span::styled("lazy ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{}:", error_count),
                Style::default().fg(theme.error),
            ),
            Span::styled("err ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{}:", modified_count),
                Style::default().fg(theme.warning),
            ),
            Span::styled("mod", Style::default().fg(theme.muted)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.muted)),
        );
        f.render_widget(title, chunks[0]);

        let header = Row::new(
            ["", "Plugin", "Mode", "Merge", "Rev", "I B A", "Detail"]
                .iter()
                .map(|h| {
                    Cell::from(*h)
                        .style(Style::default().fg(theme.info).add_modifier(Modifier::BOLD))
                }),
        )
        .style(Style::default().bg(theme.header_background))
        .height(1)
        .bottom_margin(1);

        let mut rows: Vec<Row> = Vec::with_capacity(config.plugins.len() + 1);

        if let Some(global) = hooks.global {
            // [ Global hooks ] sentinel 行: per-plugin の I/B/A 表記と揃えて、
            // init.lua は Neovim 本体の path、before/after は <config_root> 配下。
            // 存在チェックは HookCache::scan 済みなので、ここでは stat しない。
            let (hooks_text, hooks_color) = global.render(icons, theme);
            rows.push(Row::new(vec![
                Cell::from(icons.installed).style(Style::default().fg(theme.info)),
                Cell::from("[ Global hooks ]")
                    .style(Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
                Cell::from("-").style(Style::default().fg(theme.muted)),
                Cell::from("-").style(Style::default().fg(theme.muted)),
                Cell::from("-").style(Style::default().fg(theme.muted)),
                Cell::from(hooks_text).style(Style::default().fg(hooks_color)),
                Cell::from("nvim init.lua + global before/after.lua")
                    .style(Style::default().fg(theme.muted)),
            ]));
        }

        rows.extend(config.plugins.iter().enumerate().map(|(idx, p)| {
            // インストール状態アイコン
            let install_status = self
                .status_map
                .get(&p.url)
                .cloned()
                .unwrap_or(PluginStatus::Waiting);
            let (inst_icon, inst_color) = match &install_status {
                PluginStatus::Finished => (icons.installed, theme.success),
                PluginStatus::Failed(m) if m == "Missing" => (icons.missing, theme.error),
                PluginStatus::Failed(_) => (icons.failed, theme.error),
                // update 失敗マーカー: clone は健全なので赤 [Error] とは別に、
                // 黄色い failed アイコンで「前回 update がコケた」ことを示す。
                PluginStatus::UpdateFailed(_) => (icons.failed, theme.warning),
                PluginStatus::Syncing(m) if m.contains("Modified") => {
                    (icons.modified, theme.warning)
                }
                PluginStatus::Syncing(_) => (icons.syncing, theme.info),
                PluginStatus::Waiting => (icons.waiting, theme.muted),
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
                    (trg.join(" "), theme.muted)
                }
                PluginStatus::Failed(msg) => (msg.clone(), theme.error),
                PluginStatus::UpdateFailed(msg) => (format!("update failed: {msg}"), theme.warning),
                PluginStatus::Syncing(msg) => (msg.clone(), theme.warning),
                PluginStatus::Waiting => ("Checking...".to_string(), theme.muted),
            };

            let mode = if p.dev {
                ("Dev", theme.accent)
            } else if p.lazy {
                ("Lazy", theme.warning)
            } else {
                ("Eager", theme.success)
            };
            let merged = if p.merge {
                (icons.installed, theme.info)
            } else {
                ("-", theme.muted)
            };
            let rev = p.rev.as_deref().unwrap_or("-");

            // I B A 列: init/before/after.lua の存在チェック。stat は
            // HookCache::scan で config 読み込み時に済ませてある。cache 長が
            // config とズレていても行は落とさず、hook 無しとして描く (resilience)。
            let (hooks_text, hooks_color) = hooks
                .plugins
                .get(idx)
                .copied()
                .unwrap_or_default()
                .render(icons, theme);

            Row::new(vec![
                Cell::from(inst_icon).style(Style::default().fg(inst_color)),
                Cell::from(p.display_name()).style(Style::default().fg(theme.foreground)),
                Cell::from(mode.0).style(Style::default().fg(mode.1)),
                Cell::from(merged.0).style(Style::default().fg(merged.1)),
                Cell::from(rev).style(Style::default().fg(theme.accent)),
                Cell::from(hooks_text).style(Style::default().fg(hooks_color)),
                Cell::from(detail_text).style(Style::default().fg(detail_color)),
            ])
        }));

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
                .border_style(Style::default().fg(theme.info)),
        )
        .row_highlight_style(
            Style::default()
                .bg(theme.selection_background) // #3a3a3a — 落ち着いたダークグレー
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
                    Style::default().fg(theme.info).add_modifier(Modifier::BOLD),
                ),
                Span::styled(&self.search_input, Style::default().fg(theme.foreground)),
                Span::styled(
                    "\u{2588}", // █ カーソル
                    Style::default().fg(theme.info),
                ),
                Span::styled(match_info, Style::default().fg(theme.muted)),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.info)),
            )
        } else {
            Paragraph::new(Line::from(vec![
                Span::styled(" b", Style::default().fg(theme.info)),
                Span::styled(":browse ", Style::default().fg(theme.muted)),
                Span::styled("c", Style::default().fg(theme.info)),
                Span::styled(":config ", Style::default().fg(theme.muted)),
                Span::styled("e", Style::default().fg(theme.info)),
                Span::styled(":edit ", Style::default().fg(theme.muted)),
                Span::styled("s", Style::default().fg(theme.info)),
                Span::styled(":set ", Style::default().fg(theme.muted)),
                Span::styled("t", Style::default().fg(theme.info)),
                Span::styled(":tune ", Style::default().fg(theme.muted)),
                Span::styled("T", Style::default().fg(theme.info)),
                Span::styled(":theme ", Style::default().fg(theme.muted)),
                Span::styled("S", Style::default().fg(theme.info)),
                Span::styled(":sync ", Style::default().fg(theme.muted)),
                Span::styled("u/U", Style::default().fg(theme.info)),
                Span::styled(":update ", Style::default().fg(theme.muted)),
                Span::styled("d", Style::default().fg(theme.info)),
                Span::styled(":delete ", Style::default().fg(theme.muted)),
                Span::styled("/", Style::default().fg(theme.info)),
                Span::styled(":search ", Style::default().fg(theme.muted)),
                Span::styled("?", Style::default().fg(theme.info)),
                Span::styled(":help ", Style::default().fg(theme.muted)),
                Span::styled("q", Style::default().fg(theme.info)),
                Span::styled(":quit", Style::default().fg(theme.muted)),
            ]))
            .block(Block::default().borders(Borders::ALL))
        };
        f.render_widget(footer, chunks[2]);

        // ── Help popup overlay ──
        if self.show_help {
            let help_lines = vec![
                Line::from(vec![Span::styled(
                    "  Navigation",
                    Style::default().fg(theme.info).add_modifier(Modifier::BOLD),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  j / k       ", Style::default().fg(theme.info)),
                    Span::styled("Move down / up", Style::default().fg(theme.foreground)),
                ]),
                Line::from(vec![
                    Span::styled("  g / G       ", Style::default().fg(theme.info)),
                    Span::styled("Go to top / bottom", Style::default().fg(theme.foreground)),
                ]),
                Line::from(vec![
                    Span::styled("  C-d / C-u   ", Style::default().fg(theme.info)),
                    Span::styled("Half page down / up", Style::default().fg(theme.foreground)),
                ]),
                Line::from(vec![
                    Span::styled("  C-f / C-b   ", Style::default().fg(theme.info)),
                    Span::styled("Full page down / up", Style::default().fg(theme.foreground)),
                ]),
                Line::from(vec![
                    Span::styled("  / n N       ", Style::default().fg(theme.info)),
                    Span::styled(
                        "Search / next / prev",
                        Style::default().fg(theme.foreground),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  Actions",
                    Style::default().fg(theme.info).add_modifier(Modifier::BOLD),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  b           ", Style::default().fg(theme.info)),
                    Span::styled(
                        "Switch to browse TUI",
                        Style::default().fg(theme.foreground),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  c           ", Style::default().fg(theme.info)),
                    Span::styled("Open config.toml", Style::default().fg(theme.foreground)),
                ]),
                Line::from(vec![
                    Span::styled("  e           ", Style::default().fg(theme.info)),
                    Span::styled("Edit hooks", Style::default().fg(theme.foreground)),
                ]),
                Line::from(vec![
                    Span::styled("  s           ", Style::default().fg(theme.info)),
                    Span::styled("Set plugin options", Style::default().fg(theme.foreground)),
                ]),
                Line::from(vec![
                    Span::styled("  t           ", Style::default().fg(theme.info)),
                    Span::styled(
                        "Tune (AI refine selected)",
                        Style::default().fg(theme.foreground),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  T           ", Style::default().fg(theme.info)),
                    Span::styled(
                        "Pick theme preset (live preview)",
                        Style::default().fg(theme.foreground),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  S           ", Style::default().fg(theme.info)),
                    Span::styled("Sync all", Style::default().fg(theme.foreground)),
                ]),
                Line::from(vec![
                    Span::styled("  R           ", Style::default().fg(theme.info)),
                    Span::styled("Sync all (rebuild)", Style::default().fg(theme.foreground)),
                ]),
                Line::from(vec![
                    Span::styled("  u / U       ", Style::default().fg(theme.info)),
                    Span::styled(
                        "Update selected / all",
                        Style::default().fg(theme.foreground),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  d           ", Style::default().fg(theme.info)),
                    Span::styled("Delete selected", Style::default().fg(theme.foreground)),
                ]),
                Line::from(vec![
                    Span::styled("  q / Esc     ", Style::default().fg(theme.info)),
                    Span::styled("Quit", Style::default().fg(theme.foreground)),
                ]),
            ];

            // 高さは help_lines 数 + 上下 border 2 行。外枠が area を超えないようにクランプ。
            let area = f.area();
            let popup_w = 48u16.min(area.width.saturating_sub(4));
            let popup_h = (help_lines.len() as u16 + 2).min(area.height.saturating_sub(4));
            let popup = Rect::new(
                (area.width.saturating_sub(popup_w)) / 2,
                (area.height.saturating_sub(popup_h)) / 2,
                popup_w,
                popup_h,
            );

            // ポップアップの外側も含め全画面を暗くしてから、その上にポップアップを
            // 描く。一覧がそのまま透けて見えると「本当にモーダルが開いているのか」
            // 読み取りづらいという指摘 (#dim-overlay) に対応。
            f.render_widget(Clear, area);
            f.render_widget(
                Block::default().style(Style::default().bg(theme.header_background)),
                area,
            );
            f.render_widget(Clear, popup);
            f.render_widget(Block::default().style(theme.base_style()), popup);
            f.render_widget(
                Paragraph::new(help_lines).block(
                    Block::default()
                        .title(" Help [?] ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme.info)),
                ),
                popup,
            );
        }

        // ── Theme picker overlay ──
        // アクティブなプリセットのライブプレビューは関数冒頭で `theme` を
        // 差し替え済みなので、ここでは一覧のオーバーレイを描くだけでよい。
        if let Some(picker) = &self.theme_picker {
            let items: Vec<ListItem> = picker
                .presets
                .iter()
                .enumerate()
                .map(|(i, (name, _, is_custom))| {
                    let label = if *is_custom {
                        format!("{name} (custom)")
                    } else {
                        name.clone()
                    };
                    let style = if i == picker.selected {
                        Style::default()
                            .fg(theme.foreground)
                            .bg(theme.selection_background)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.foreground)
                    };
                    ListItem::new(format!(" {label}")).style(style)
                })
                .collect();

            let area = f.area();
            let popup_w = 40u16.min(area.width.saturating_sub(4));
            let popup_h = (picker.presets.len() as u16 + 4).min(area.height.saturating_sub(4));
            let popup = Rect::new(
                (area.width.saturating_sub(popup_w)) / 2,
                (area.height.saturating_sub(popup_h)) / 2,
                popup_w,
                popup_h,
            );

            // 全画面を暗くしてからポップアップを描く (help ポップアップと同じ理由)。
            f.render_widget(Clear, area);
            f.render_widget(
                Block::default().style(Style::default().bg(theme.header_background)),
                area,
            );
            f.render_widget(Clear, popup);
            f.render_widget(Block::default().style(theme.base_style()), popup);
            let (title, border) = if picker.confirming {
                (" Theme [T] — confirm ", theme.warning)
            } else {
                (" Theme [T] ", theme.info)
            };
            let mut list_state = ListState::default().with_selected(Some(picker.selected));
            f.render_stateful_widget(
                List::new(items).block(
                    Block::default()
                        .title(title)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border)),
                ),
                popup,
                &mut list_state,
            );
            let hint_y = (popup.y + popup.height).min(area.height.saturating_sub(1));
            if picker.confirming {
                // 確認プロンプトは popup 幅 (40) に収まらないので frame 全幅を
                // 使う。書き込む対象 (`[options.theme]`) と preset 名を明示して、
                // 「今の色が丸ごと消える」ことが読めるようにする。
                let name = picker.current().map(|(n, _, _)| n.as_str()).unwrap_or("");
                let hint_area = Rect::new(0, hint_y, area.width, 1);
                f.render_widget(Clear, hint_area);
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(
                            format!(" Overwrite [options.theme] with '{name}'? "),
                            Style::default().fg(theme.warning),
                        ),
                        Span::styled(
                            "Enter/y:apply  Esc/n:cancel",
                            Style::default().fg(theme.muted),
                        ),
                    ]))
                    .style(theme.base_style()),
                    hint_area,
                );
            } else {
                let hint_area = Rect::new(popup.x, hint_y, popup.width, 1);
                f.render_widget(
                    Paragraph::new(Line::from(vec![Span::styled(
                        " j/k:preview  Enter:apply  Esc:cancel",
                        Style::default().fg(theme.muted),
                    )])),
                    hint_area,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_sync_and_list_render_custom_statuses_and_selection() {
        let config = crate::config::parse_config("[options.theme]\nsuccess = 101\nerror = 102\nwarning = 103\ninfo = 104\nmuted = 105\nselection_background = 106\nbackground = 107\n[[plugins]]\nurl = 'owner/plugin'").unwrap();
        let theme = config.options.theme;
        let icons = Icons::from_style(crate::config::IconStyle::Ascii);
        let hooks = HookCache {
            global: None,
            plugins: vec![HookFlags::default()],
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        let mut state = TuiState::new(vec!["owner/plugin".into()]);
        for (status, color) in [
            (PluginStatus::Waiting, theme.muted),
            (PluginStatus::Syncing("Fetching".into()), theme.info),
            (PluginStatus::Finished, theme.success),
            (PluginStatus::Failed("Failed".into()), theme.error),
            (
                PluginStatus::UpdateFailed("Update failed".into()),
                theme.warning,
            ),
        ] {
            state.update_status("owner/plugin", status);
            terminal
                .draw(|f| state.draw(f, "syncing", &icons, &theme))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(2, 4)].fg, color);
            assert_eq!(buffer[(2, 4)].bg, theme.selection_background);
            terminal
                .draw(|f| state.draw_list(f, &config, &icons, &hooks, &theme))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(3, 6)].fg, color);
            assert_eq!(buffer[(3, 6)].bg, theme.selection_background);
        }
        state.start_search();
        state.search_type('p');
        state.show_help = true;
        terminal
            .draw(|f| state.draw_list(f, &config, &icons, &hooks, &theme))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(50, 20)].bg, theme.background);
    }

    #[test]
    fn theme_list_header_uses_configured_background() {
        let config =
            crate::config::parse_config("[options.theme]\nheader_background = 52").unwrap();
        let mut state = TuiState::new(Vec::new());
        let icons = Icons::from_style(crate::config::IconStyle::Ascii);
        let hooks = HookCache {
            global: None,
            plugins: Vec::new(),
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|f| state.draw_list(f, &config, &icons, &hooks, &config.options.theme))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(5, 4)].bg, Color::Indexed(52));
    }

    #[test]
    fn theme_picker_state_new_selects_active_name_or_falls_back_to_first() {
        let presets = vec![
            ("dracula".to_string(), crate::theme::Theme::default(), false),
            ("nord".to_string(), crate::theme::Theme::default(), false),
        ];
        let picker = ThemePickerState::new(presets.clone(), Some("nord"));
        assert_eq!(picker.selected, 1);
        assert_eq!(picker.current().unwrap().0, "nord");

        let picker = ThemePickerState::new(presets.clone(), Some("not-listed"));
        assert_eq!(
            picker.selected, 0,
            "unknown active name falls back to first"
        );

        let picker = ThemePickerState::new(presets, None);
        assert_eq!(picker.selected, 0);
        assert!(
            !picker.confirming,
            "a freshly opened picker must not be armed to overwrite [options.theme]"
        );
    }

    #[test]
    fn theme_picker_confirm_must_be_armed_before_applying_and_navigation_disarms_it() {
        let presets = vec![
            ("a".to_string(), crate::theme::Theme::default(), false),
            ("b".to_string(), crate::theme::Theme::default(), false),
        ];
        let mut picker = ThemePickerState::new(presets, None);

        picker.request_confirm();
        assert!(picker.confirming, "Enter arms the overwrite confirmation");

        picker.cancel_confirm();
        assert!(!picker.confirming, "Esc / n backs out of the confirmation");

        // 確認中に選択を動かしたら解除される — プロンプトに出ていた preset と
        // 別の preset が書き込まれるのを防ぐため。
        picker.request_confirm();
        picker.next();
        assert!(!picker.confirming, "moving down cancels a pending confirm");
        picker.request_confirm();
        picker.previous();
        assert!(!picker.confirming, "moving up cancels a pending confirm");
    }

    #[test]
    fn theme_picker_confirm_cannot_be_armed_with_no_presets() {
        let mut picker = ThemePickerState::new(Vec::new(), None);
        picker.request_confirm();
        assert!(
            !picker.confirming,
            "there is nothing to apply, so nothing to confirm"
        );
    }

    #[test]
    fn theme_picker_state_next_previous_wrap_around() {
        let presets = vec![
            ("a".to_string(), crate::theme::Theme::default(), false),
            ("b".to_string(), crate::theme::Theme::default(), false),
            ("c".to_string(), crate::theme::Theme::default(), false),
        ];
        let mut picker = ThemePickerState::new(presets, None);
        assert_eq!(picker.selected, 0);
        picker.previous();
        assert_eq!(
            picker.selected, 2,
            "previous from 0 wraps to the last entry"
        );
        picker.next();
        assert_eq!(picker.selected, 0, "next from the last entry wraps to 0");
        picker.next();
        assert_eq!(picker.selected, 1);
    }

    #[test]
    fn theme_picker_state_next_previous_are_no_ops_when_empty() {
        let mut picker = ThemePickerState::new(Vec::new(), None);
        picker.next();
        picker.previous();
        assert_eq!(picker.selected, 0);
        assert!(picker.current().is_none());
    }

    #[test]
    fn theme_picker_live_preview_recolors_the_whole_frame_without_touching_config() {
        // Moving the picker selection must repaint using the *previewed*
        // preset's colors, while `config.options.theme` (the persisted
        // theme) stays untouched until Enter is pressed elsewhere.
        let config = crate::config::parse_config("[options]").unwrap();
        let persisted_theme = config.options.theme;
        let dracula = crate::theme::builtin_preset("dracula").unwrap();
        let nord = crate::theme::builtin_preset("nord").unwrap();
        assert_ne!(dracula.background, nord.background);
        assert_ne!(persisted_theme.background, dracula.background);

        let icons = Icons::from_style(crate::config::IconStyle::Ascii);
        let hooks = HookCache {
            global: None,
            plugins: Vec::new(),
        };
        let mut state = TuiState::new(Vec::new());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();

        state.theme_picker = Some(ThemePickerState {
            presets: vec![
                ("dracula".to_string(), dracula, false),
                ("nord".to_string(), nord, false),
            ],
            selected: 0,
            confirming: false,
        });
        terminal
            .draw(|f| state.draw_list(f, &config, &icons, &hooks, &persisted_theme))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 0)].bg, dracula.background);

        state.theme_picker.as_mut().unwrap().selected = 1;
        terminal
            .draw(|f| state.draw_list(f, &config, &icons, &hooks, &persisted_theme))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 0)].bg, nord.background);

        // Config itself was never mutated by navigating the preview.
        assert_eq!(config.options.theme, persisted_theme);

        state.theme_picker = None;
        terminal
            .draw(|f| state.draw_list(f, &config, &icons, &hooks, &persisted_theme))
            .unwrap();
        assert_eq!(
            terminal.backend().buffer()[(0, 0)].bg,
            persisted_theme.background,
            "closing the picker without applying restores the persisted theme"
        );
    }

    #[test]
    fn theme_picker_confirming_draws_the_overwrite_prompt() {
        // 破壊的な書き込みの前に、何が上書きされるのかが画面に出ていること。
        let config = crate::config::parse_config("[options]").unwrap();
        let persisted_theme = config.options.theme;
        let icons = Icons::from_style(crate::config::IconStyle::Ascii);
        let hooks = HookCache {
            global: None,
            plugins: Vec::new(),
        };
        let mut state = TuiState::new(Vec::new());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();

        state.theme_picker = Some(ThemePickerState {
            presets: vec![(
                "nord".to_string(),
                crate::theme::builtin_preset("nord").unwrap(),
                false,
            )],
            selected: 0,
            confirming: false,
        });
        let rendered = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };

        terminal
            .draw(|f| state.draw_list(f, &config, &icons, &hooks, &persisted_theme))
            .unwrap();
        assert!(
            !rendered(&terminal).contains("Overwrite [options.theme]"),
            "no prompt before Enter arms the confirmation"
        );

        state.theme_picker.as_mut().unwrap().request_confirm();
        terminal
            .draw(|f| state.draw_list(f, &config, &icons, &hooks, &persisted_theme))
            .unwrap();
        let out = rendered(&terminal);
        assert!(
            out.contains("Overwrite [options.theme] with 'nord'?"),
            "the pending overwrite must name the target and the preset; got:\n{out}"
        );
    }

    #[test]
    fn theme_picker_dims_the_full_screen_behind_the_popup() {
        // Regression: the popup used to only `Clear` its own rect, leaving
        // the plugin list fully legible (and, on terminals that don't fully
        // repaint the alternate screen, stale glyphs from the pane behind
        // it) all around the popup. The list is now dimmed with a full-area
        let config = crate::config::parse_config(
            "[options]\n\n[[plugins]]\nurl = \"owner/some-very-long-plugin-name\"\n",
        )
        .unwrap();
        let theme = config.options.theme;
        let icons = Icons::from_style(crate::config::IconStyle::Ascii);
        let hooks = HookCache {
            global: None,
            plugins: Vec::new(),
        };
        let mut state = TuiState::new(vec![config.plugins[0].url.clone()]);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();

        // Corner cell, far outside where the centered popup will land.
        terminal
            .draw(|f| state.draw_list(f, &config, &icons, &hooks, &theme))
            .unwrap();
        assert_ne!(
            terminal.backend().buffer().cell((0, 0)).unwrap().bg,
            theme.header_background,
            "sanity: corner isn't already header_background before the picker opens"
        );

        state.theme_picker = Some(ThemePickerState {
            presets: vec![(
                "nord".to_string(),
                crate::theme::builtin_preset("nord").unwrap(),
                false,
            )],
            selected: 0,
            confirming: false,
        });
        terminal
            .draw(|f| state.draw_list(f, &config, &icons, &hooks, &theme))
            .unwrap();
        let corner = terminal.backend().buffer().cell((0, 0)).unwrap();
        let nord = crate::theme::builtin_preset("nord").unwrap();
        assert_eq!(
            corner.bg, nord.header_background,
            "area outside the popup must be dimmed (using the live-previewed preset's color) while the theme picker is open"
        );
    }

    #[test]
    fn theme_picker_scrolls_so_the_selection_stays_visible() {
        // R1-2-2: with more presets than the clamped popup height can show,
        // a `List` rendered without `ListState` never scrolls, so selecting
        // past the last visible row leaves the highlighted preset (and its
        // name) permanently off-screen. `render_stateful_widget` +
        // `ListState::with_selected` is what makes ratatui compute a scroll
        // offset that keeps the selected row in view.
        let config = crate::config::parse_config("[options]").unwrap();
        let persisted_theme = config.options.theme;
        let icons = Icons::from_style(crate::config::IconStyle::Ascii);
        let hooks = HookCache {
            global: None,
            plugins: Vec::new(),
        };
        let presets: Vec<(String, crate::theme::Theme, bool)> = (0..30)
            .map(|i| {
                (
                    format!("preset-{i:02}"),
                    crate::theme::Theme::default(),
                    true,
                )
            })
            .collect();
        let last = presets.len() - 1;
        let mut state = TuiState::new(Vec::new());
        // Small terminal: the popup height is clamped well below
        // `presets.len()`, so this only passes if the widget actually
        // scrolls rather than always drawing from the top of the list.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 15)).unwrap();

        state.theme_picker = Some(ThemePickerState {
            presets,
            selected: last,
            confirming: false,
        });
        terminal
            .draw(|f| state.draw_list(f, &config, &icons, &hooks, &persisted_theme))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            rendered.contains(&format!("preset-{last:02}")),
            "selecting the last preset must scroll it into view; got:\n{rendered}"
        );
    }

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
        let theme = crate::theme::Theme::default();
        assert_eq!(TuiState::progress_color(0.0, &theme), theme.error);
        assert_eq!(TuiState::progress_color(0.24, &theme), theme.error);
        assert_eq!(TuiState::progress_color(0.25, &theme), theme.warning);
        assert_eq!(TuiState::progress_color(0.49, &theme), theme.warning);
        assert_eq!(TuiState::progress_color(0.5, &theme), theme.info);
        assert_eq!(TuiState::progress_color(0.74, &theme), theme.info);
        assert_eq!(TuiState::progress_color(0.75, &theme), theme.success);
        assert_eq!(TuiState::progress_color(1.0, &theme), theme.success);
    }

    #[test]
    fn hook_cache_scan_reflects_files_on_disk() {
        use crate::config::{Config, Plugin};
        let tmp = tempfile::tempdir().unwrap();
        let config_root = tmp.path();

        // global: before.lua だけ置く (init.lua は nvim 側の path を別に渡す)
        std::fs::write(config_root.join("before.lua"), "").unwrap();

        // plugin A: init.lua と after.lua あり / plugin B: hook 無し
        let a = Plugin {
            url: "owner/a.nvim".to_string(),
            ..Default::default()
        };
        let b = Plugin {
            url: "owner/b.nvim".to_string(),
            ..Default::default()
        };
        let adir = crate::paths::resolve_plugin_config_dir(config_root, &a);
        std::fs::create_dir_all(&adir).unwrap();
        std::fs::write(adir.join("init.lua"), "").unwrap();
        std::fs::write(adir.join("after.lua"), "").unwrap();

        let config = Config {
            vars: None,
            options: crate::config::Options::default(),
            plugins: vec![a, b],
        };

        let init_lua = config_root.join("nvim-init.lua");
        std::fs::write(&init_lua, "").unwrap();
        let hooks = HookCache::scan(&config, config_root, Some(&init_lua));

        assert_eq!(
            hooks.global,
            Some(HookFlags {
                init: true,
                before: true,
                after: false,
            })
        );
        assert_eq!(
            hooks.plugins,
            vec![
                HookFlags {
                    init: true,
                    before: false,
                    after: true,
                },
                HookFlags::default(),
            ]
        );

        // sentinel を出さない呼び出しでは global を走査しない
        let no_global = HookCache::scan(&config, config_root, None);
        assert_eq!(no_global.global, None);
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
