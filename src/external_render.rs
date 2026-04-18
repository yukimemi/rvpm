//! store TUI の README preview を外部コマンドに委譲するためのヘルパー。
//!
//! `options.store.readme_command` に指定されたコマンド列を spawn し、
//! stdin に raw markdown を流し、stdout の ANSI エスケープを `ansi-to-tui`
//! で `ratatui::text::Text<'static>` に変換して返す。
//!
//! 失敗 / timeout / 空出力のときは `Ok(None)` を返し、呼び出し側は
//! 内蔵 tui-markdown パイプラインに fallback する。
//!
//! コマンド引数内で使える placeholder (すべて optional):
//! - `{width}` — pane 内側幅 (列)
//! - `{height}` — pane 内側高さ (行)
//! - `{file_path}` — raw markdown を書き出した temp ファイル絶対パス
//! - `{file_dir}` — `{file_path}` の親ディレクトリ
//!
//! `{file_path}` が使われた場合は stdin を close して (空 stdin として) 渡す。
//! そうでない場合は raw markdown を stdin に pipe する。

use anyhow::Result;
use ratatui::text::Text;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// subprocess の strict な実行上限。これを超えた場合は kill して fallback へ。
const RENDER_TIMEOUT: Duration = Duration::from_secs(3);

/// 指定された外部コマンドで `markdown` を整形し、結果を `Text<'static>` で返す。
///
/// 戻り値:
/// - `Ok(Some(text))` 成功
/// - `Ok(None)` コマンド未指定 / 空出力 / exit != 0 (呼び出し側は fallback)
/// - `Err(_)` spawn 失敗や I/O エラー (呼び出し側は warning 出して fallback)
pub fn render(
    command: &[String],
    markdown: &str,
    width: u16,
    height: u16,
) -> Result<Option<Text<'static>>> {
    if command.is_empty() {
        return Ok(None);
    }

    // `{file_path}` / `{file_dir}` を使う場合は tempfile を用意する。
    // 一度展開してみて placeholder が含まれているかどうか判定。
    let needs_file = command
        .iter()
        .any(|a| a.contains("{file_path}") || a.contains("{file_dir}"));
    let tempfile_holder = if needs_file {
        let mut f = tempfile::Builder::new()
            .prefix("rvpm-store-readme-")
            .suffix(".md")
            .tempfile()?;
        f.write_all(markdown.as_bytes())?;
        f.flush()?;
        Some(f)
    } else {
        None
    };

    let (args, use_stdin) = expand_args(command, width, height, tempfile_holder.as_ref());

    let Some((program, rest)) = args.split_first() else {
        return Ok(None);
    };

    let mut cmd = Command::new(program);
    cmd.args(rest);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    cmd.stdin(if use_stdin {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let mut child = cmd.spawn()?;
    if use_stdin && let Some(mut stdin) = child.stdin.take() {
        // 書き込み失敗は子プロセスが早期終了した等で起きるので無視。
        let _ = stdin.write_all(markdown.as_bytes());
    }

    // wait-timeout で hard deadline を設ける。
    use wait_timeout::ChildExt;
    let status = match child.wait_timeout(RENDER_TIMEOUT)? {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
    };

    if !status.success() {
        return Ok(None);
    }

    let mut stdout = child.stdout.take();
    let mut buf = Vec::new();
    if let Some(mut s) = stdout.take() {
        use std::io::Read;
        s.read_to_end(&mut buf)?;
    }
    if buf.is_empty() {
        return Ok(None);
    }

    use ansi_to_tui::IntoText;
    let text = buf.into_text().map_err(anyhow::Error::from)?;
    Ok(Some(text_to_owned(text)))
}

/// 引数列の placeholder を展開し、stdin が必要かどうかのフラグを返す。
fn expand_args(
    command: &[String],
    width: u16,
    height: u16,
    tempfile: Option<&tempfile::NamedTempFile>,
) -> (Vec<String>, bool) {
    let (file_path, file_dir) = match tempfile {
        Some(f) => {
            let p = f.path().to_string_lossy().to_string();
            let dir = f
                .path()
                .parent()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_default();
            (p, dir)
        }
        None => (String::new(), String::new()),
    };
    let use_stdin = tempfile.is_none();

    let expanded: Vec<String> = command
        .iter()
        .map(|a| {
            a.replace("{width}", &width.to_string())
                .replace("{height}", &height.to_string())
                .replace("{file_path}", &file_path)
                .replace("{file_dir}", &file_dir)
        })
        .collect();
    (expanded, use_stdin)
}

/// ansi-to-tui が返す `Text<'_>` を所有権付き `Text<'static>` に変換する。
/// キャッシュして毎フレーム clone したいので 'static にしておく必要がある。
fn text_to_owned(text: Text<'_>) -> Text<'static> {
    use ratatui::text::{Line, Span};
    let lines: Vec<Line<'static>> = text
        .lines
        .into_iter()
        .map(|line| {
            let spans: Vec<Span<'static>> = line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content.into_owned(), span.style))
                .collect();
            let mut l = Line::from(spans);
            l.style = line.style;
            l.alignment = line.alignment;
            l
        })
        .collect();
    let mut out = Text::from(lines);
    out.style = text.style;
    out.alignment = text.alignment;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_command_returns_none() {
        let result = render(&[], "# hi", 80, 24).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_expand_args_substitutes_width_height() {
        let cmd = vec![
            "x".to_string(),
            "--cols={width}".to_string(),
            "--rows={height}".to_string(),
        ];
        let (expanded, use_stdin) = expand_args(&cmd, 120, 40, None);
        assert_eq!(
            expanded,
            vec![
                "x".to_string(),
                "--cols=120".to_string(),
                "--rows=40".to_string(),
            ]
        );
        assert!(use_stdin);
    }

    #[test]
    fn test_expand_args_file_path_uses_tempfile_and_no_stdin() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cmd = vec!["x".to_string(), "{file_path}".to_string()];
        let (expanded, use_stdin) = expand_args(&cmd, 80, 24, Some(&tmp));
        assert_eq!(expanded[0], "x");
        assert_eq!(expanded[1], tmp.path().to_string_lossy());
        assert!(!use_stdin);
    }

    /// 実コマンド: `echo` で ANSI 出力 1 行を吐かせて取り込む smoke test。
    /// Windows では `cmd /c echo` で試す。
    #[test]
    #[cfg(unix)]
    fn test_render_with_echo_smoke() {
        let cmd = vec!["echo".to_string(), "hello".to_string()];
        let text = render(&cmd, "", 80, 24).unwrap();
        let text = text.expect("echo should produce output");
        let joined: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(joined.contains("hello"));
    }
}
