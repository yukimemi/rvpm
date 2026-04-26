// AI-assisted `rvpm add` (#93).
//
// このモジュールは静的 scan (#90, plugin_scan.rs) の代わりに外部 AI CLI
// (claude / gemini / codex) を呼んで `[[plugins]]` 全体 + 必要な hook ファイル
// を提案させる。設計トレードオフ:
//
//   - **CLI subprocess 経由**: API key 管理を user の `claude login` / `gemini auth`
//     に委ねる。SDK 直叩きより薄く保ち、3 ツール統一インターフェース。
//   - **構造化出力**: AI 出力は `<rvpm:plugin_entry>` 等の XML tag で囲ませ、
//     code fence や前置きが混ざっても robust に regex 抽出する。
//   - **Mode A (内蔵 chat loop)** がメイン路: rvpm が会話履歴を保持し毎ターン
//     `claude -p "..."` を一発投げ直す。長期会話は token 食うが TOML 抽出が
//     確実 + 3 ツール挙動統一。
//   - **Mode B (handoff)** は user に CLI を直接渡す逃げ道: prompt をテンポラリ
//     ファイルに保存してパスを announce、`claude` (interactive) を inherit-stdio
//     で spawn する。**stdin 事前注入はしない** (claude-code は EOF で即 exit する
//     ため interactive にならない)。user が CLI 内で prompt ファイルを読めば
//     refine 済み文脈が手に入る。CLI ツール側のファイル編集機能で config.toml /
//     hook 直接書かせる。**rvpm 側は結果を re-import しない** (README に明記)。

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};

mod chat;
mod prompt;

pub use chat::{ChatOutcome, run_ai_add, run_ai_tune};

/// 利用可能な AI CLI ツール。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Claude,
    Gemini,
    Codex,
}

/// `crate::config::AiBackend` (Off を含む TOML 設定型) → 実行時 `Backend` 変換。
///
/// `AiBackend::Off` は AI 機能を使わない指示なので runtime backend には変換できず
/// `Err(())` を返す。Caller (`run_add` / `run_tune`) は Off 判定を済ませた後に
/// 呼び出すか、`Err` を見て non-AI 経路に分岐する。
impl TryFrom<crate::config::AiBackend> for Backend {
    type Error = ();
    fn try_from(value: crate::config::AiBackend) -> std::result::Result<Self, ()> {
        match value {
            crate::config::AiBackend::Claude => Ok(Backend::Claude),
            crate::config::AiBackend::Gemini => Ok(Backend::Gemini),
            crate::config::AiBackend::Codex => Ok(Backend::Codex),
            crate::config::AiBackend::Off => Err(()),
        }
    }
}

impl Backend {
    /// CLI 実行ファイル名 (PATH 上にあるべきもの)。
    pub fn cli_name(self) -> &'static str {
        match self {
            Backend::Claude => "claude",
            Backend::Gemini => "gemini",
            Backend::Codex => "codex",
        }
    }

    /// `cli_name()` が PATH 上に見つかるかを返す。Windows では `.ps1` 等の
    /// non-default PATHEXT も探すので、pnpm 等が `.ps1` wrapper のみ install した
    /// ケースでも検出できる。
    pub fn is_available(self) -> bool {
        resolve_cli(self.cli_name()).is_some()
    }

    /// バックエンド共通のラベル。
    pub fn label(self) -> &'static str {
        match self {
            Backend::Claude => "Claude",
            Backend::Gemini => "Gemini",
            Backend::Codex => "Codex",
        }
    }
}

/// 解決された CLI の起動情報。
#[derive(Debug, Clone)]
pub struct ResolvedCli {
    /// `Command::new` に渡すプログラム (`.exe` 直、もしくは `powershell.exe`)。
    pub program: PathBuf,
    /// プログラムの最初に付ける引数 (PowerShell 経由時は `-File <path>` 等)。
    pub prefix_args: Vec<String>,
}

/// `name` を PATH から解決する (Windows の `.ps1` 対応込み)。
///
/// 探索順:
///   1. `which(name)` — Unix なら直接、Windows なら PATHEXT デフォルト (`.exe`/`.cmd`/`.bat` 等)。
///      解決パスが `.ps1` だった場合は PowerShell 起動命令として包む。
///   2. (Windows のみ) `which("name.ps1")` — pnpm 等が `.ps1` のみ install した
///      ケースの fallback。PATHEXT に `.ps1` が無くても拾える。
///
/// `.ps1` を実行するには Windows の `CreateProcess` 単体では不可なので、
/// `powershell.exe -NoProfile -ExecutionPolicy Bypass -File <full path>` で wrap する。
pub fn resolve_cli(name: &str) -> Option<ResolvedCli> {
    if let Ok(p) = which::which(name) {
        return Some(wrap_if_powershell(p));
    }
    #[cfg(windows)]
    {
        for ext in ["ps1", "cmd", "bat", "exe"] {
            if let Ok(p) = which::which(format!("{name}.{ext}")) {
                return Some(wrap_if_powershell(p));
            }
        }
    }
    None
}

fn wrap_if_powershell(path: PathBuf) -> ResolvedCli {
    let is_ps1 = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("ps1"))
        .unwrap_or(false);
    if is_ps1 {
        // PowerShell 7 (`pwsh.exe`) を優先、無ければ Windows PowerShell 5.1
        // (`powershell.exe`、Windows 標準同梱) に fallback。
        //   - pnpm 利用層は modern toolchain に偏るので PS7 入りが大多数。
        //   - pnpm の wrapper script は単純な PATH/exec 操作のみで、PS5.1 / 7
        //     どちらでも同じ挙動 (silent fallback の behavioral risk が無い)。
        //   - PS5.1 を primary にすると PS7 のみ user で odd ハマる可能性。
        //
        // `-NoProfile` で user の $PROFILE をスキップ (起動高速化 + side effect 排除)、
        // `-ExecutionPolicy Bypass` で署名要求と zone-prompt の両方を無効化
        // (Unrestricted は MOTW タグ付き script で interactive prompt を出すので、
        // subprocess 起動時に hang する可能性がある)。
        let ps_exe = if which::which("pwsh").is_ok() {
            "pwsh.exe"
        } else {
            "powershell.exe"
        };
        ResolvedCli {
            program: PathBuf::from(ps_exe),
            prefix_args: vec![
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                path.to_string_lossy().into_owned(),
            ],
        }
    } else {
        ResolvedCli {
            program: path,
            prefix_args: Vec::new(),
        }
    }
}

/// 1 セクション (例: `after.lua`) に対して AI が返す 2 案。
///
/// - `fresh`: 既存ファイルを **無視して** ゼロから書いた場合の提案。`None` は AI が
///   `(none)` を返した (= 何も書かなくて良い) を意味する。
/// - `merged`: 既存ファイル本文を AI に見せた上で **マージした** 提案。`None` は
///   「既存が prompt に渡されていなかった」or「AI が merged tag を返さなかった」の
///   どちらか。caller 側で同義に扱える (どちらにせよ merged は提示しない)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProposalSection {
    pub fresh: Option<String>,
    pub merged: Option<String>,
}

impl ProposalSection {
    /// fresh も merged も無い (AI 提案が一切ない) ならば true。
    pub fn is_empty(&self) -> bool {
        self.fresh.is_none() && self.merged.is_none()
    }
}

/// AI が出力する 1 ターン分の提案。
///
/// 各セクションは `fresh` (greenfield) と `merged` (既存と統合) の 2 案を持つ。
/// `[[plugins]]` block は最低 1 つ (fresh または merged) が必須、hook 系は両方
/// `None` なら何も書かない。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Proposal {
    /// `[[plugins]]` block (TOML として valid であることを `validate_proposal_toml` で検証)。
    /// `add` では `fresh` のみ、`tune` では `fresh` + `merged` 両方が想定される。
    pub plugin_entry: ProposalSection,
    pub init_lua: ProposalSection,
    pub before_lua: ProposalSection,
    pub after_lua: ProposalSection,
    /// 2-3 文の根拠説明 (preview 表示用)。
    pub explanation: String,
}

/// AI CLI が PATH に無いときのエラー (install hint 込み)。
pub fn ensure_cli_installed(backend: Backend) -> Result<()> {
    if backend.is_available() {
        return Ok(());
    }
    let cli = backend.cli_name();
    let hint = match backend {
        Backend::Claude => "https://docs.claude.com/claude-code",
        Backend::Gemini => "https://ai.google.dev/gemini-api/docs/cli",
        Backend::Codex => "https://github.com/openai/codex",
    };
    Err(anyhow!(
        "AI backend `{cli}` is not on PATH. Install it first ({hint}) or pass a different `--ai` flag."
    ))
}

/// CLI を一発呼び出しモードで起動して prompt を投げ、応答を文字列で返す。
/// stdin で prompt を渡す (shell escape & 長文対策)。
///
/// **timeout**: 5 分 (300 秒)。当初 90 秒だったが、chat 2 turn 目以降は
/// `build_followup_prompt` で `initial + prior_response + feedback` を全部
/// 再投入するので prompt が 50-100KB クラスに膨らみ、Gemini が 90 秒では
/// 収まらないケース報告あり。300 秒なら現実的に余裕がある。
/// `RVPM_AI_TIMEOUT_SECS` 環境変数で per-call 上書き可能 (ネットワーク遅延が
/// 強い環境向け)。
pub async fn invoke_oneshot(backend: Backend, prompt_text: &str) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    use tokio::time::{Duration, timeout};

    ensure_cli_installed(backend)?;
    let resolved = resolve_cli(backend.cli_name())
        .ok_or_else(|| anyhow!("AI CLI `{}` is not on PATH", backend.cli_name()))?;

    // prompt サイズを表示 (timeout 原因の透明性 + sanity check)。
    eprintln!(
        "  (prompt size: {} bytes / {} lines)",
        prompt_text.len(),
        prompt_text.lines().count()
    );

    // 各 CLI のフラグは「stdin から prompt を読み、結果を stdout に」のモードを選ぶ:
    //   - claude: `claude -p` で one-shot non-interactive、stdin で prompt
    //   - gemini: `gemini -p` 同様
    //   - codex:  `codex exec`  (or `codex -p`、ver 依存)
    // どれも stdin 受け付けるはず。安全側に prompt を stdin で渡す。
    let mut cmd = Command::new(&resolved.program);
    cmd.args(&resolved.prefix_args);
    match backend {
        Backend::Claude | Backend::Gemini => {
            cmd.arg("-p").arg("-");
        }
        Backend::Codex => {
            cmd.arg("exec").arg("-");
        }
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // tokio Command の `kill_on_drop` は default false。timeout で future が
        // drop されたとき子プロセスを残さないように true にする (CodeRabbit Critical)。
        .kill_on_drop(true);

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "failed to spawn AI CLI `{}` (is it installed and on PATH?)",
            backend.cli_name()
        )
    })?;

    // stdin に prompt を書き込んで close (EOF を AI に伝える)。
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt_text.as_bytes())
            .await
            .context("failed to write prompt to AI CLI stdin")?;
        // explicit drop → close stdin → AI が EOF 受け取って応答開始
    }

    // timeout は default 300 秒、`RVPM_AI_TIMEOUT_SECS` で上書き可能。
    let timeout_secs = std::env::var("RVPM_AI_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(300);
    let output = timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow!(
                "AI CLI `{}` timed out after {timeout_secs}s. \
                 The chat follow-up prompt grows with conversation history; \
                 set RVPM_AI_TIMEOUT_SECS=600 or longer if your network is slow.",
                backend.cli_name()
            )
        })?
        .with_context(|| format!("AI CLI `{}` failed to produce output", backend.cli_name()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "AI CLI `{}` exited with status {}: {}",
            backend.cli_name(),
            output.status,
            stderr.trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// AI 応答から `<rvpm:plugin_entry>` 等の XML tag を抜き取る。
///
/// 各セクションは `<rvpm:NAME>` (fresh) と `<rvpm:NAME_merged>` (merged) を
/// 並列に探す。`<rvpm:plugin_entry>` (= fresh) は最低でも fresh / merged の
/// **どちらか** が存在しないとエラー。hook 系 (`init_lua` 等) は両方無くても OK。
pub fn parse_proposal(response: &str) -> Result<Proposal> {
    let plugin_entry = extract_section(response, "plugin_entry");
    if plugin_entry.is_empty() {
        return Err(anyhow!(
            "AI response missing required <rvpm:plugin_entry> (or <rvpm:plugin_entry_merged>) tag"
        ));
    }
    let init_lua = extract_section(response, "init_lua");
    let before_lua = extract_section(response, "before_lua");
    let after_lua = extract_section(response, "after_lua");
    let explanation =
        extract_tag(response, "explanation").unwrap_or_else(|| "(no explanation given)".into());
    Ok(Proposal {
        plugin_entry,
        init_lua,
        before_lua,
        after_lua,
        explanation: explanation.trim().to_string(),
    })
}

/// `<rvpm:NAME>` (fresh) と `<rvpm:NAME_merged>` (merged) の両方を抽出する。
///
/// 中身が空 / `(none)` のタグは `None` 扱いに正規化。`plugin_entry` も他の
/// セクションも同じ規則 (TOML 本文に `(none)` を書く意味は無いので衝突しない)。
fn extract_section(response: &str, name: &str) -> ProposalSection {
    let fresh = extract_optional_section(response, name);
    let merged_tag = format!("{name}_merged");
    let merged = extract_optional_section(response, &merged_tag);
    ProposalSection { fresh, merged }
}

/// セクション本文を `Option<String>` で返す。`(none)` (大文字小文字無視) や空は `None`。
fn extract_optional_section(text: &str, name: &str) -> Option<String> {
    let body = extract_tag(text, name)?;
    let trimmed = body.trim();
    if trimmed.eq_ignore_ascii_case("(none)") || trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `<rvpm:NAME>...</rvpm:NAME>` の中身を返す (前後 whitespace つき)。
/// 見つからなければ `None`。
///
/// AI の preamble に `<rvpm:plugin_entry>` という単語が混じる false positive を避けるため、
/// **最後の occurrence** を起点に matching する: 構造化出力は応答末尾に来るのが
/// 通常だし、preamble の言及で偶発的に block を切り出す事故が起きにくい。
fn extract_tag(text: &str, name: &str) -> Option<String> {
    let open = format!("<rvpm:{name}>");
    let close = format!("</rvpm:{name}>");
    let start_off = text.rfind(&open)? + open.len();
    let close_off = text[start_off..].find(&close)? + start_off;
    Some(text[start_off..close_off].to_string())
}

/// AI 提案 TOML が valid であることを軽く verify (parse できるか + `[[plugins]]`
/// が 1 件あるか)。
pub fn validate_proposal_toml(toml_src: &str) -> Result<()> {
    let value: toml::Value =
        toml::from_str(toml_src).context("AI-proposed TOML failed to parse")?;
    let plugins = value
        .get("plugins")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("AI proposal missing `[[plugins]]` array"))?;
    if plugins.is_empty() {
        return Err(anyhow!("AI proposal contains 0 plugin entries"));
    }
    if plugins.len() > 1 {
        return Err(anyhow!(
            "AI proposed {} plugin entries; expected exactly 1 for `rvpm add`",
            plugins.len()
        ));
    }
    Ok(())
}

/// CLI 起動時に最初の user message をどう注入するかの戦略。各 backend の
/// interactive 起動時の引数仕様に合わせて切り替える。
#[derive(Debug, Clone, Copy)]
enum FirstMessageStrategy {
    /// `<cli> "<msg>"` — positional arg で interactive を継続したまま最初の
    /// user message を送る。claude / codex 両方この方式。
    Positional,
    /// `gemini -i "<msg>"` — `--prompt-interactive` 相当の short flag。
    /// `-p` (non-interactive) と区別して対話継続する。
    InteractiveFlag,
    /// Auto-send が安全に出来ない backend 用フォールバック。argless で
    /// interactive 起動 + stderr に「コピペしてください」案内を出す。現状
    /// 未使用だが将来 backend を増やした際の保険として残す。
    #[allow(dead_code)]
    Manual,
}

fn first_message_strategy(backend: Backend) -> FirstMessageStrategy {
    match backend {
        // claude: `claude "<msg>"`
        // codex:  `codex  "<msg>"` (claude と同様 positional 仕様)
        Backend::Claude | Backend::Codex => FirstMessageStrategy::Positional,
        // gemini: `gemini -i "<msg>"` (`-p` だと one-shot non-interactive)
        Backend::Gemini => FirstMessageStrategy::InteractiveFlag,
    }
}

/// Mode B のハンドオフ: prompt をテンポラリファイルに書き出し、CLI を
/// **interactive モードで** 起動する (stdin / stdout / stderr とも親 TTY を継承)。
/// rvpm は CLI 終了まで `wait` するだけで、それ以降の状態は CLI 側に委譲する。
///
/// **prompt の事前注入 (stdin pipe) はしない**: stdin pipe + drop すると claude-code
/// などは EOF を受けて即座に exit するため、interactive にならない。
/// 代わりに prompt をテンポラリ MD ファイルに保存してパスを announce する。
///
/// **CLI 別の "first message" 戦略**: `first_message_strategy` を参照。
///   - claude: positional arg
///   - gemini: `-i <msg>` flag
///   - codex: コピペ案内のみ (interactive-first-message の安定 flag が無いため)
///
/// CLI 終了まで blocking wait するが、`spawn_blocking` で別 thread に逃がして
/// Tokio executor は塞がない。
pub async fn run_handoff(backend: Backend, prompt_text: &str) -> Result<()> {
    use console::style;

    ensure_cli_installed(backend)?;
    let resolved = resolve_cli(backend.cli_name())
        .ok_or_else(|| anyhow!("AI CLI `{}` is not on PATH", backend.cli_name()))?;

    // prompt をテンポラリファイルに保存
    let mut tmp_path = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    tmp_path.push(format!("rvpm-ai-prompt-{stamp}.md"));
    std::fs::write(&tmp_path, prompt_text)
        .with_context(|| format!("failed to write prompt to {}", tmp_path.display()))?;

    let path_str = tmp_path.to_string_lossy().into_owned();
    // **Passive な指示にする** — 旧 wording は "apply parts ... refine ... revise" と
    // 動詞先行で書いていたため、claude code 等が「読み終えたらすぐ適用していい」と
    // 解釈して handoff 直後に勝手に config.toml / hook を書き換える事故が起きた
    // (user 報告)。ここでは「まず読むだけ」「内容を 1-2 行で要約」「次の指示を
    // 待て」の 3 点を明示し、最初の turn でファイル編集ツールを呼ばないように釘を刺す。
    let first_message = format!(
        "Read the file at `{path_str}` for our shared context — it contains \
         rvpm's full context plus the latest proposal as XML tags \
         (`<rvpm:plugin_entry>`, `<rvpm:after_lua>`, etc.).\n\n\
         **Important: do NOT apply, edit, or write any files yet.** \
         After reading, briefly acknowledge what you found (1-2 sentences \
         summarizing the proposal) and then **wait for my next instruction**. \
         I will tell you which parts to apply, what to refine, or how to \
         revise. Don't run any Edit / Write tools until I explicitly ask."
    );

    eprintln!();
    eprintln!(
        "\u{1f4dd} Hand-off prompt saved to:\n   {}",
        style(&path_str).cyan()
    );
    eprintln!();

    let strategy = first_message_strategy(backend);
    if matches!(strategy, FirstMessageStrategy::Manual) {
        eprintln!(
            "Starting `{}` interactively. Once the prompt opens, paste this as \
             your first message:\n",
            backend.cli_name()
        );
        eprintln!("  {}", style(&first_message).bold());
        eprintln!();
    } else {
        eprintln!(
            "Starting `{}` interactively. The first message asking it to read \
             the file above will be sent automatically.\n",
            backend.cli_name()
        );
    }

    // 子プロセスを spawn_blocking 内で起動 + wait (std::process は async に乗らないため)。
    let label = backend.cli_name().to_string();
    let program = resolved.program.clone();
    let prefix_args = resolved.prefix_args.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&prefix_args);
        match strategy {
            FirstMessageStrategy::Positional => {
                // claude "<msg>"
                cmd.arg(&first_message);
            }
            FirstMessageStrategy::InteractiveFlag => {
                // gemini -i "<msg>"
                cmd.arg("-i").arg(&first_message);
            }
            FirstMessageStrategy::Manual => {
                // no extra args — user pastes the message after the prompt opens
            }
        }
        let status = cmd
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .with_context(|| format!("failed to spawn AI CLI `{label}`"))?;
        let _ = status; // exit status は無視 (user 操作なのでエラーじゃない)
        Ok(())
    })
    .await
    .context("failed to join blocking handoff task")??;

    Ok(())
}

/// 1 hook ファイル (例: `after.lua`) に対する user の最終選択。
///
/// chat loop の `print_proposal_preview` + per-section dialog で確定し、
/// `write_hook_files` がそのまま実行する。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HookChoice {
    /// 何も書かない (既存ファイルがあれば保持、無ければ作らない)。
    #[default]
    Keep,
    /// 指定の body をファイルに書く (既存があれば上書き)。chat loop が `fresh` /
    /// `merged` どちらを選ぶかは事前に解決済みで、ここには本文だけが渡る。
    Write(String),
}

impl HookChoice {
    pub fn body(&self) -> Option<&str> {
        match self {
            HookChoice::Keep => None,
            HookChoice::Write(s) => Some(s.as_str()),
        }
    }
}

/// `write_hook_files` の引数。3 hook ファイル分の決定を持つ。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookWriteDecisions {
    pub init_lua: HookChoice,
    pub before_lua: HookChoice,
    pub after_lua: HookChoice,
}

/// AI mode で生成された hook 内容を、呼び出し側で resolve 済みの per-plugin
/// config dir (`<config_root>/plugins/<host>/<owner>/<repo>/`) に書き込む。
///
/// `decisions` は chat loop 側で per-file に user が確定した選択 (Keep / Write)。
/// Write を選んだファイルは既存があっても **上書き** する: user は preview で
/// 「Use fresh (overwrite)」「Use merged (overwrite)」を明示選択した前提なので、
/// ここで silent skip すると user の選択を裏切ることになる。
///
/// `chezmoi_enabled` (`options.chezmoi`) が true のとき、書き込みは `chezmoi::write_path`
/// 経由で source state に行い、`chezmoi::apply` で target に反映する。
/// raw `fs::write` で target に直書きすると次の `chezmoi apply` で削除/drift 扱いになるため、
/// `rvpm edit` 等の他コマンドと同じ規約に揃える。
///
/// path 解決は呼び出し側 (`run_add`) が `resolve_plugin_config_dir` 経由で行う。
/// ここでホスト名や url 形式を再パースしないことで、GitLab / 他 host や
/// `Plugin::canonical_path` の形式変更にも自動追従する。
pub async fn write_hook_files(
    plugin_dir: &Path,
    decisions: &HookWriteDecisions,
    chezmoi_enabled: bool,
) -> Result<Vec<PathBuf>> {
    // 全 Keep なら作業ディレクトリの create も不要 (no-op)。
    if matches!(decisions.init_lua, HookChoice::Keep)
        && matches!(decisions.before_lua, HookChoice::Keep)
        && matches!(decisions.after_lua, HookChoice::Keep)
    {
        return Ok(Vec::new());
    }

    std::fs::create_dir_all(plugin_dir).with_context(|| {
        format!(
            "failed to create plugin config dir {}",
            plugin_dir.display()
        )
    })?;

    let mut written = Vec::new();
    for (name, choice) in [
        ("init.lua", &decisions.init_lua),
        ("before.lua", &decisions.before_lua),
        ("after.lua", &decisions.after_lua),
    ] {
        let Some(body) = choice.body() else { continue };
        let target = plugin_dir.join(name);
        crate::chezmoi::write_routed(chezmoi_enabled, &target, format!("{}\n", body.trim_end()))
            .await
            .with_context(|| format!("failed to write {}", target.display()))?;
        written.push(target);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_try_from_aibackend_maps_runtime_variants() {
        use crate::config::AiBackend as Cfg;
        assert_eq!(Backend::try_from(Cfg::Claude), Ok(Backend::Claude));
        assert_eq!(Backend::try_from(Cfg::Gemini), Ok(Backend::Gemini));
        assert_eq!(Backend::try_from(Cfg::Codex), Ok(Backend::Codex));
        assert_eq!(Backend::try_from(Cfg::Off), Err(()));
    }

    #[test]
    fn parse_proposal_extracts_required_tags() {
        let response = r#"
some preamble that should be ignored

<rvpm:plugin_entry>
[[plugins]]
url = "owner/repo"
on_cmd = ["Foo"]
</rvpm:plugin_entry>

<rvpm:init_lua>
vim.g.foo = 1
</rvpm:init_lua>

<rvpm:before_lua>(none)</rvpm:before_lua>
<rvpm:after_lua>
require('foo').setup({})
</rvpm:after_lua>

<rvpm:explanation>
README shows :Foo as the entry command.
</rvpm:explanation>
"#;
        let p = parse_proposal(response).unwrap();
        let entry_fresh = p.plugin_entry.fresh.as_deref().unwrap();
        assert!(entry_fresh.contains("[[plugins]]"));
        assert!(entry_fresh.contains(r#"url = "owner/repo""#));
        assert!(p.plugin_entry.merged.is_none(), "no _merged tag was sent");
        assert_eq!(p.init_lua.fresh.as_deref(), Some("vim.g.foo = 1"));
        assert!(p.before_lua.fresh.is_none(), "(none) must collapse to None");
        assert_eq!(
            p.after_lua.fresh.as_deref(),
            Some("require('foo').setup({})")
        );
        assert!(p.explanation.contains("README shows"));
    }

    #[test]
    fn parse_proposal_extracts_merged_variants_when_present() {
        // tune-style response: AI returns both fresh and merged for hook files
        // and for the [[plugins]] entry.
        let response = r#"
<rvpm:plugin_entry>
[[plugins]]
url = "owner/repo"
on_cmd = ["Foo"]
</rvpm:plugin_entry>

<rvpm:plugin_entry_merged>
[[plugins]]
url = "owner/repo"
on_cmd = ["Foo", "FooBar"]
rev = "v1.0"
</rvpm:plugin_entry_merged>

<rvpm:init_lua>(none)</rvpm:init_lua>
<rvpm:init_lua_merged>(none)</rvpm:init_lua_merged>

<rvpm:before_lua>vim.g.foo_new = 1</rvpm:before_lua>
<rvpm:before_lua_merged>
vim.g.foo_old = "user"
vim.g.foo_new = 1
</rvpm:before_lua_merged>

<rvpm:after_lua>require('foo').setup({})</rvpm:after_lua>
<rvpm:after_lua_merged>
require('foo').setup({})
vim.keymap.set("n", "<leader>f", ":Foo<CR>")
</rvpm:after_lua_merged>

<rvpm:explanation>tune proposal.</rvpm:explanation>
"#;
        let p = parse_proposal(response).unwrap();
        // plugin_entry: both fresh and merged
        assert!(
            p.plugin_entry
                .fresh
                .as_deref()
                .unwrap()
                .contains("[[plugins]]")
        );
        let merged_entry = p.plugin_entry.merged.as_deref().unwrap();
        assert!(merged_entry.contains(r#"rev = "v1.0""#));
        assert!(merged_entry.contains("FooBar"));
        // init_lua: both (none) → both None
        assert!(p.init_lua.fresh.is_none());
        assert!(p.init_lua.merged.is_none());
        // before_lua: fresh + merged differ
        assert_eq!(p.before_lua.fresh.as_deref(), Some("vim.g.foo_new = 1"));
        assert!(
            p.before_lua
                .merged
                .as_deref()
                .unwrap()
                .contains("vim.g.foo_old")
        );
        // after_lua: merged adds keymap that fresh doesn't have
        assert!(p.after_lua.fresh.as_deref().unwrap().contains("setup({})"));
        assert!(
            p.after_lua
                .merged
                .as_deref()
                .unwrap()
                .contains("vim.keymap.set")
        );
    }

    #[test]
    fn parse_proposal_accepts_only_merged_when_fresh_missing() {
        // Defensive: if the AI returns ONLY the merged variant (no fresh) we still
        // accept it — the chat preview will surface "no fresh available" so user
        // sees what's offered. This matters because Codex / Claude / Gemini may
        // each occasionally drop a tag.
        let response = r#"
<rvpm:plugin_entry_merged>
[[plugins]]
url = "owner/repo"
</rvpm:plugin_entry_merged>
<rvpm:explanation>only merged available.</rvpm:explanation>
"#;
        let p = parse_proposal(response).unwrap();
        assert!(p.plugin_entry.fresh.is_none());
        assert!(p.plugin_entry.merged.is_some());
    }

    #[test]
    fn parse_proposal_missing_plugin_entry_errors() {
        let response = "<rvpm:explanation>nothing else</rvpm:explanation>";
        assert!(parse_proposal(response).is_err());
    }

    #[test]
    fn parse_proposal_ignores_tag_name_in_preamble() {
        // AI が "I will use the <rvpm:plugin_entry> tag below..." のように preamble で
        // tag 名を言及するケース。最後の occurrence を起点にすれば誤切り出しを回避できる。
        let response = r#"
I will populate the <rvpm:plugin_entry> tag below with the proposal.

<rvpm:plugin_entry>
[[plugins]]
url = "real/entry"
</rvpm:plugin_entry>
<rvpm:init_lua>(none)</rvpm:init_lua>
<rvpm:before_lua>(none)</rvpm:before_lua>
<rvpm:after_lua>(none)</rvpm:after_lua>
<rvpm:explanation>ok</rvpm:explanation>
"#;
        let p = parse_proposal(response).unwrap();
        let entry = p.plugin_entry.fresh.as_deref().unwrap();
        assert!(entry.contains("real/entry"));
        assert!(!entry.contains("populate"));
    }

    #[test]
    fn parse_proposal_extracts_when_wrapped_in_markdown_fences() {
        // 一部 CLI は ``` fence を勝手に付ける可能性。tag 抽出は中身さえあれば OK。
        let response = r#"
```
<rvpm:plugin_entry>
[[plugins]]
url = "x/y"
</rvpm:plugin_entry>
<rvpm:init_lua>(none)</rvpm:init_lua>
<rvpm:before_lua>(none)</rvpm:before_lua>
<rvpm:after_lua>(none)</rvpm:after_lua>
<rvpm:explanation>ok</rvpm:explanation>
```
"#;
        let p = parse_proposal(response).unwrap();
        assert!(
            p.plugin_entry
                .fresh
                .as_deref()
                .unwrap()
                .contains(r#"url = "x/y""#)
        );
        assert!(p.init_lua.fresh.is_none());
    }

    #[test]
    fn validate_proposal_toml_accepts_single_plugin_entry() {
        let toml_src = r#"
[[plugins]]
url = "owner/repo"
on_cmd = ["Foo"]
"#;
        validate_proposal_toml(toml_src).unwrap();
    }

    #[test]
    fn validate_proposal_toml_rejects_multiple_plugin_entries() {
        let toml_src = r#"
[[plugins]]
url = "a/b"

[[plugins]]
url = "c/d"
"#;
        assert!(validate_proposal_toml(toml_src).is_err());
    }

    #[test]
    fn validate_proposal_toml_rejects_invalid_syntax() {
        let toml_src = "[[plugins]\nurl = ";
        assert!(validate_proposal_toml(toml_src).is_err());
    }

    #[test]
    fn validate_proposal_toml_rejects_no_plugins_array() {
        let toml_src = r#"name = "ignored""#;
        assert!(validate_proposal_toml(toml_src).is_err());
    }

    #[test]
    fn wrap_if_powershell_wraps_ps1_path() {
        // .ps1 ファイルは pwsh.exe (PS7 入りなら) または powershell.exe で起動。
        // どちらが選ばれるかは test 実行環境に依存するので exact 比較しない。
        let p = std::path::PathBuf::from("C:/foo/gemini.ps1");
        let r = wrap_if_powershell(p);
        let prog = r.program.to_string_lossy().to_ascii_lowercase();
        assert!(
            prog == "pwsh.exe" || prog == "powershell.exe",
            "expected pwsh.exe or powershell.exe, got {prog}"
        );
        assert!(r.prefix_args.iter().any(|a| a == "-File"));
        assert!(r.prefix_args.iter().any(|a| a.contains("gemini.ps1")));
        // ExecutionPolicy Bypass で署名要求 + zone prompt を無効化する
        assert!(r.prefix_args.iter().any(|a| a == "Bypass"));
        // -NoProfile で user $PROFILE スキップ
        assert!(r.prefix_args.iter().any(|a| a == "-NoProfile"));
    }

    #[test]
    fn wrap_if_powershell_passes_exe_through() {
        // .exe は直接起動 (prefix_args 空)。
        let p = std::path::PathBuf::from("C:/foo/claude.exe");
        let r = wrap_if_powershell(p.clone());
        assert_eq!(r.program, p);
        assert!(r.prefix_args.is_empty());
    }

    #[test]
    fn wrap_if_powershell_passes_unix_path_through() {
        // 拡張子無し (Unix の典型的な executable) も直接起動。
        let p = std::path::PathBuf::from("/usr/local/bin/codex");
        let r = wrap_if_powershell(p.clone());
        assert_eq!(r.program, p);
        assert!(r.prefix_args.is_empty());
    }

    #[tokio::test]
    async fn write_hook_files_writes_only_files_with_write_decision() {
        // 呼び出し側 (`run_add`) が plugin_dir を resolve 済みで渡す前提を確認。
        // chezmoi_enabled=false で従来 path (raw fs::write 相当) と挙動一致するか。
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp
            .path()
            .join("plugins")
            .join("github.com")
            .join("o")
            .join("r");
        let decisions = HookWriteDecisions {
            init_lua: HookChoice::Write("vim.g.x = 1".to_string()),
            before_lua: HookChoice::Keep,
            after_lua: HookChoice::Write("require('o').setup({})".to_string()),
        };
        let written = write_hook_files(&plugin_dir, &decisions, false)
            .await
            .unwrap();
        assert_eq!(written.len(), 2);
        assert!(plugin_dir.join("init.lua").exists());
        assert!(!plugin_dir.join("before.lua").exists());
        assert!(plugin_dir.join("after.lua").exists());
    }

    #[tokio::test]
    async fn write_hook_files_overwrites_existing_when_user_chose_write() {
        // user が「Use fresh / Use merged」を選んだら overwrite するのが新仕様。
        // 旧実装は silent skip していたが、user の選択を尊重するようになった。
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("p");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("after.lua"), "OLD CONTENT\n").unwrap();
        let decisions = HookWriteDecisions {
            after_lua: HookChoice::Write("NEW CONTENT".to_string()),
            ..Default::default()
        };
        write_hook_files(&plugin_dir, &decisions, false)
            .await
            .unwrap();
        let body = std::fs::read_to_string(plugin_dir.join("after.lua")).unwrap();
        assert_eq!(body, "NEW CONTENT\n");
    }

    #[tokio::test]
    async fn write_hook_files_keep_does_not_touch_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("p");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("after.lua"), "USER\n").unwrap();
        let decisions = HookWriteDecisions::default(); // all Keep
        let written = write_hook_files(&plugin_dir, &decisions, false)
            .await
            .unwrap();
        assert!(written.is_empty());
        let body = std::fs::read_to_string(plugin_dir.join("after.lua")).unwrap();
        assert_eq!(body, "USER\n");
    }

    #[test]
    fn extract_optional_section_collapses_none_marker() {
        let resp = "<rvpm:init_lua>  (none)  </rvpm:init_lua>";
        assert_eq!(extract_optional_section(resp, "init_lua"), None);
    }

    #[test]
    fn extract_optional_section_keeps_real_content() {
        let resp = "<rvpm:init_lua>vim.g.x = 1</rvpm:init_lua>";
        assert_eq!(
            extract_optional_section(resp, "init_lua").as_deref(),
            Some("vim.g.x = 1")
        );
    }
}
