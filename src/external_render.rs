//! store TUI の README preview を外部コマンドに委譲するためのヘルパー。
//!
//! `options.store.readme_command` に指定されたコマンド列を spawn し、
//! stdin に raw markdown を流し、stdout の ANSI エスケープを `ansi-to-tui`
//! で `ratatui::text::Text<'static>` に変換して返す。
//!
//! 失敗 / timeout / 空出力のときは `Ok(None)` を返し、呼び出し側は
//! 内蔵 tui-markdown パイプラインに fallback する。
//!
//! コマンド引数内で使える placeholder は **Tera 風の `{{ name }}` 記法** で
//! 書く (rvpm 他箇所の `[vars]` / template と統一)。空白有無は任意:
//! - `{{ width }}` — pane 内側幅 (列)
//! - `{{ height }}` — pane 内側高さ (行)
//! - `{{ file_path }}` — raw markdown を書き出した temp ファイル絶対パス
//! - `{{ file_dir }}` — `{{ file_path }}` の親ディレクトリ
//! - `{{ file_name }}` — `{{ file_path }}` のファイル名部分
//! - `{{ file_stem }}` — `{{ file_name }}` から拡張子を除いた部分
//! - `{{ file_ext }}` — 拡張子 (dot 無し、例: `md`)
//!
//! `{{ file_* }}` のいずれかが使われた場合は stdin を close して (空 stdin
//! として) 渡す。そうでない場合は raw markdown を stdin に pipe する。

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

    // `{{ file_* }}` のいずれかを使う場合は tempfile を用意する。
    let needs_file = command.iter().any(|a| uses_file_placeholder(a));
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

/// 引数列の `{{ name }}` placeholder を展開し、stdin が必要かどうかのフラグを返す。
fn expand_args(
    command: &[String],
    width: u16,
    height: u16,
    tempfile: Option<&tempfile::NamedTempFile>,
) -> (Vec<String>, bool) {
    use std::path::Path;

    let (file_path, file_dir, file_name, file_stem, file_ext) = match tempfile {
        Some(f) => {
            let p: &Path = f.path();
            let s = |o: Option<&std::ffi::OsStr>| {
                o.map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default()
            };
            let sp = |o: Option<&Path>| {
                o.map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            };
            (
                p.to_string_lossy().to_string(),
                sp(p.parent()),
                s(p.file_name()),
                s(p.file_stem()),
                s(p.extension()),
            )
        }
        None => (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ),
    };
    let use_stdin = tempfile.is_none();

    let vars: [(&str, &str); 7] = [
        ("width", &width_str(width)),
        ("height", &height_str(height)),
        ("file_path", &file_path),
        ("file_dir", &file_dir),
        ("file_name", &file_name),
        ("file_stem", &file_stem),
        ("file_ext", &file_ext),
    ];
    let expanded: Vec<String> = command.iter().map(|a| substitute(a, &vars)).collect();
    (expanded, use_stdin)
}

// 一時 String を借用するためのヘルパー (配列リテラル内で直接 `&value.to_string()` と書けない)
fn width_str(w: u16) -> String {
    w.to_string()
}
fn height_str(h: u16) -> String {
    h.to_string()
}

/// 文字列内の `{{ name }}` (空白有無は任意) を `vars` で置換する。
/// 未知の名前は置換せずそのまま残す。`{{` と `}}` は ASCII なので byte index
/// で slicing しても UTF-8 境界は壊れない。
fn substitute(s: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{'
            && bytes[i + 1] == b'{'
            && let Some(end_rel) = s[i + 2..].find("}}")
        {
            let inner_start = i + 2;
            let inner_end = inner_start + end_rel;
            let key = s[inner_start..inner_end].trim();
            if let Some((_, val)) = vars.iter().find(|(k, _)| *k == key) {
                out.push_str(&s[last..i]);
                out.push_str(val);
                i = inner_end + 2;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[last..]);
    out
}

/// 引数に `{{ file_path }}` / `{{ file_dir }}` / `{{ file_name }}` / ... の
/// いずれかが含まれるかを判定する (空白無視)。1 つでもあれば tempfile が必要。
fn uses_file_placeholder(arg: &str) -> bool {
    const FILE_KEYS: &[&str] = &[
        "file_path",
        "file_dir",
        "file_name",
        "file_stem",
        "file_ext",
    ];
    let bytes = arg.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{'
            && bytes[i + 1] == b'{'
            && let Some(end_rel) = arg[i + 2..].find("}}")
        {
            let key = arg[i + 2..i + 2 + end_rel].trim();
            if FILE_KEYS.contains(&key) {
                return true;
            }
            i = i + 2 + end_rel + 2;
            continue;
        }
        i += 1;
    }
    false
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
            "--cols={{ width }}".to_string(),
            "--rows={{height}}".to_string(), // 空白無しも許容
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
        let tmp = tempfile::Builder::new()
            .prefix("test-")
            .suffix(".md")
            .tempfile()
            .unwrap();
        let cmd = vec!["x".to_string(), "{{ file_path }}".to_string()];
        let (expanded, use_stdin) = expand_args(&cmd, 80, 24, Some(&tmp));
        assert_eq!(expanded[0], "x");
        assert_eq!(expanded[1], tmp.path().to_string_lossy());
        assert!(!use_stdin);
    }

    #[test]
    fn test_expand_args_all_file_placeholders() {
        let tmp = tempfile::Builder::new()
            .prefix("readme-")
            .suffix(".md")
            .tempfile()
            .unwrap();
        let cmd = vec![
            "--path={{ file_path }}".to_string(),
            "--dir={{ file_dir }}".to_string(),
            "--name={{ file_name }}".to_string(),
            "--stem={{ file_stem }}".to_string(),
            "--ext={{ file_ext }}".to_string(),
        ];
        let (expanded, use_stdin) = expand_args(&cmd, 80, 24, Some(&tmp));
        let name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let stem = tmp
            .path()
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(expanded[0], format!("--path={}", tmp.path().display()));
        assert!(expanded[1].starts_with("--dir="));
        assert_eq!(expanded[2], format!("--name={}", name));
        assert_eq!(expanded[3], format!("--stem={}", stem));
        assert_eq!(expanded[4], "--ext=md");
        assert!(!use_stdin);
    }

    #[test]
    fn test_expand_args_unknown_placeholder_left_as_is() {
        let cmd = vec!["{{ nonexistent }}".to_string(), "{{ width }}".to_string()];
        let (expanded, _) = expand_args(&cmd, 100, 20, None);
        assert_eq!(expanded[0], "{{ nonexistent }}");
        assert_eq!(expanded[1], "100");
    }

    #[test]
    fn test_substitute_multiple_occurrences_in_one_arg() {
        let vars = [("a", "foo"), ("b", "bar")];
        assert_eq!(substitute("{{a}}/{{b}}/{{a}}", &vars), "foo/bar/foo");
        assert_eq!(substitute("{{ a }} and {{ b }}", &vars), "foo and bar");
    }

    #[test]
    fn test_uses_file_placeholder_detects_any_variant() {
        assert!(uses_file_placeholder("--in={{ file_path }}"));
        assert!(uses_file_placeholder("{{file_dir}}"));
        assert!(uses_file_placeholder("--out={{file_name}}"));
        assert!(uses_file_placeholder("{{ file_stem }}"));
        assert!(uses_file_placeholder("{{ file_ext }}"));
        assert!(!uses_file_placeholder("{{ width }}"));
        assert!(!uses_file_placeholder("no placeholders here"));
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
