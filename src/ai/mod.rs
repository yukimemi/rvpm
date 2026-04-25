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

pub use chat::{ChatOutcome, run_ai_add};

/// 利用可能な AI CLI ツール。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Claude,
    Gemini,
    Codex,
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

    /// `cli_name()` が PATH 上に見つかるかを返す。
    pub fn is_available(self) -> bool {
        which::which(self.cli_name()).is_ok()
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

/// AI が出力する 1 ターン分の提案。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Proposal {
    /// `[[plugins]]` block (TOML として valid であることを呼び出し側で検証)。
    pub plugin_entry_toml: String,
    /// per-plugin init.lua 内容。`None` なら作らない。
    pub init_lua: Option<String>,
    pub before_lua: Option<String>,
    pub after_lua: Option<String>,
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
/// stdin で prompt を渡す (shell escape & 長文対策)。timeout 90 秒。
pub async fn invoke_oneshot(backend: Backend, prompt_text: &str) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    use tokio::time::{Duration, timeout};

    ensure_cli_installed(backend)?;

    // 各 CLI のフラグは「stdin から prompt を読み、結果を stdout に」のモードを選ぶ:
    //   - claude: `claude -p` で one-shot non-interactive、stdin で prompt
    //   - gemini: `gemini -p` 同様
    //   - codex:  `codex exec`  (or `codex -p`、ver 依存)
    // どれも stdin 受け付けるはず。安全側に prompt を stdin で渡す。
    let mut cmd = Command::new(backend.cli_name());
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

    // 最大 90 秒で打ち切り (network 遅延 + thinking time の余裕)。
    let output = timeout(Duration::from_secs(90), child.wait_with_output())
        .await
        .map_err(|_| anyhow!("AI CLI `{}` timed out after 90s", backend.cli_name()))?
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
pub fn parse_proposal(response: &str) -> Result<Proposal> {
    let entry = extract_tag(response, "plugin_entry")
        .ok_or_else(|| anyhow!("AI response missing required <rvpm:plugin_entry> tag"))?;
    let init = extract_optional_lua(response, "init_lua");
    let before = extract_optional_lua(response, "before_lua");
    let after = extract_optional_lua(response, "after_lua");
    let explanation =
        extract_tag(response, "explanation").unwrap_or_else(|| "(no explanation given)".into());
    Ok(Proposal {
        plugin_entry_toml: entry.trim().to_string(),
        init_lua: init,
        before_lua: before,
        after_lua: after,
        explanation: explanation.trim().to_string(),
    })
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

/// Lua 系 tag の中身を `Option<String>` で返す。`(none)` (大文字小文字無視) は `None`。
fn extract_optional_lua(text: &str, name: &str) -> Option<String> {
    let body = extract_tag(text, name)?;
    let trimmed = body.trim();
    if trimmed.eq_ignore_ascii_case("(none)") || trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
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

/// Mode B のハンドオフ: prompt をテンポラリファイルに書き出し、CLI を
/// **interactive モードで** 起動する (stdin / stdout / stderr とも親 TTY を継承)。
/// rvpm は CLI 終了まで `wait` するだけで、それ以降の状態は CLI 側に委譲する。
///
/// **prompt の事前注入は意図的に行わない**: stdin pipe + drop すると claude-code
/// などは EOF を受けて即座に exit するため、interactive にならない (Gemini High)。
/// 代わりに prompt をテンポラリ MD ファイルに保存してパスを announce する。
/// user は CLI 内で `/file <path>` (claude-code) や `cat <path>` を使って
/// 読み込めばよい。
///
/// CLI 終了まで blocking wait するが、`spawn_blocking` で別 thread に逃がして
/// Tokio executor は塞がない。
pub async fn run_handoff(backend: Backend, prompt_text: &str) -> Result<()> {
    ensure_cli_installed(backend)?;

    // prompt をテンポラリファイルに保存
    let mut tmp_path = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    tmp_path.push(format!("rvpm-ai-prompt-{stamp}.md"));
    std::fs::write(&tmp_path, prompt_text)
        .with_context(|| format!("failed to write prompt to {}", tmp_path.display()))?;

    eprintln!();
    eprintln!(
        "\u{1f4dd} Prompt saved to: {}\n\
         Starting `{}` interactively. Paste the prompt or load it via your CLI's file-reading mechanism.\n",
        tmp_path.display(),
        backend.cli_name()
    );

    // 子プロセスを spawn_blocking 内で起動 + wait (std::process は async に乗らないため)。
    let cli_name = backend.cli_name().to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let status = std::process::Command::new(&cli_name)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .with_context(|| format!("failed to spawn AI CLI `{cli_name}`"))?;
        let _ = status; // exit status は無視 (user 操作なのでエラーじゃない)
        Ok(())
    })
    .await
    .context("failed to join blocking handoff task")??;

    Ok(())
}

/// AI mode で生成された hook 内容を、呼び出し側で resolve 済みの per-plugin
/// config dir (`<config_root>/plugins/<host>/<owner>/<repo>/`) に書き込む。
///
/// path 解決は呼び出し側 (`run_add`) が `resolve_plugin_config_dir` 経由で行う。
/// ここでホスト名や url 形式を再パースしないことで、GitLab / 他 host や
/// `Plugin::canonical_path` の形式変更にも自動追従する。
pub fn write_hook_files(plugin_dir: &Path, proposal: &Proposal) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(plugin_dir).with_context(|| {
        format!(
            "failed to create plugin config dir {}",
            plugin_dir.display()
        )
    })?;

    let mut written = Vec::new();
    for (name, body) in [
        ("init.lua", proposal.init_lua.as_deref()),
        ("before.lua", proposal.before_lua.as_deref()),
        ("after.lua", proposal.after_lua.as_deref()),
    ] {
        let Some(body) = body else { continue };
        let path = plugin_dir.join(name);
        // 既存ファイルは上書きしない (user の手書き編集を尊重)。
        if path.exists() {
            eprintln!(
                "\u{26a0} {} already exists, skipping AI-generated content. Apply manually if desired.",
                path.display()
            );
            continue;
        }
        std::fs::write(&path, format!("{}\n", body.trim_end()))
            .with_context(|| format!("failed to write {}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(p.plugin_entry_toml.contains("[[plugins]]"));
        assert!(p.plugin_entry_toml.contains(r#"url = "owner/repo""#));
        assert_eq!(p.init_lua.as_deref(), Some("vim.g.foo = 1"));
        assert_eq!(p.before_lua, None, "(none) must collapse to None");
        assert_eq!(p.after_lua.as_deref(), Some("require('foo').setup({})"));
        assert!(p.explanation.contains("README shows"));
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
        assert!(p.plugin_entry_toml.contains("real/entry"));
        assert!(!p.plugin_entry_toml.contains("populate"));
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
        assert!(p.plugin_entry_toml.contains(r#"url = "x/y""#));
        assert_eq!(p.init_lua, None);
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
    fn write_hook_files_writes_only_present_lua_blocks() {
        // 呼び出し側 (`run_add`) が plugin_dir を resolve 済みで渡す前提を確認。
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp
            .path()
            .join("plugins")
            .join("github.com")
            .join("o")
            .join("r");
        let p = Proposal {
            plugin_entry_toml: r#"[[plugins]]
url = "o/r""#
                .to_string(),
            init_lua: Some("vim.g.x = 1".to_string()),
            before_lua: None,
            after_lua: Some("require('o').setup({})".to_string()),
            explanation: "test".to_string(),
        };
        let written = write_hook_files(&plugin_dir, &p).unwrap();
        assert_eq!(written.len(), 2);
        assert!(plugin_dir.join("init.lua").exists());
        assert!(!plugin_dir.join("before.lua").exists());
        assert!(plugin_dir.join("after.lua").exists());
    }

    #[test]
    fn extract_optional_lua_collapses_none_marker() {
        let resp = "<rvpm:init_lua>  (none)  </rvpm:init_lua>";
        assert_eq!(extract_optional_lua(resp, "init_lua"), None);
    }

    #[test]
    fn extract_optional_lua_keeps_real_content() {
        let resp = "<rvpm:init_lua>vim.g.x = 1</rvpm:init_lua>";
        assert_eq!(
            extract_optional_lua(resp, "init_lua").as_deref(),
            Some("vim.g.x = 1")
        );
    }
}
