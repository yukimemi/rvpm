use crate::loader::PluginScripts;
use std::path::{Path, PathBuf};

/// helptags 生成結果のレポート。
#[derive(Debug, PartialEq, Eq)]
pub struct HelptagsReport {
    /// 対象となった `doc/` ディレクトリの数。
    pub target_count: usize,
    /// 実際に `nvim --headless` を起動したか (nvim 不在時は false)。
    pub ran: bool,
    /// nvim プロセスが非 0 終了した場合の exit code。
    pub exit_code: Option<i32>,
}

/// `nvim --headless` に渡すべき `doc/` ディレクトリを列挙する。
///
/// ルール:
/// - `merged_dir/doc/` が存在すれば最初に追加 (複数プラグインの doc が 1 箇所に
///   まとまるので `:helptags` 1 回で済む)
/// - eager + merge=true なプラグインは merged/doc/ に含まれるので個別に追加しない
/// - それ以外 (lazy プラグイン全般 / merge=false な eager) は
///   `<plugin.path>/doc/` を存在チェック付きで追加
///
/// `cond` フィールドは Lua runtime 評価のため Rust 側からは判定できず、全プラグイン
/// (cond が false となるもの含む) を候補にする。`rvpm list` に現れるプラグインが
/// そのまま対象、というのが分かりやすい mental model。
pub fn collect_helptag_targets(
    plugin_scripts: &[PluginScripts],
    merged_dir: &Path,
) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    let merged_doc = merged_dir.join("doc");
    if merged_doc.is_dir() {
        targets.push(merged_doc);
    }
    for ps in plugin_scripts {
        if ps.merge && !ps.lazy {
            continue;
        }
        let doc = PathBuf::from(&ps.path).join("doc");
        if doc.is_dir() {
            targets.push(doc);
        }
    }
    targets
}

/// `:helptags <path>` を各ターゲット向けに並べた Vim script を生成する。
///
/// Windows のバックスラッシュパスは forward slash に正規化 (Vim 側で解釈可)。
/// path に含まれる single quote は `''` にエスケープ。
/// 個別失敗 (doc/ が壊れている等) で全体が止まらないよう `try/catch` でラップし、
/// 例外は `echomsg` で stderr に流す。
pub fn build_helptags_script(targets: &[PathBuf]) -> String {
    let mut script = String::new();
    for t in targets {
        let p = t.to_string_lossy().replace('\\', "/").replace('\'', "''");
        script.push_str(&format!(
            "try | execute 'helptags' fnameescape('{}') | catch | echomsg 'rvpm helptags: ' . v:exception | endtry\n",
            p
        ));
    }
    script
}

/// `sync` / `generate` 完了時に呼ばれる entry point。対象が無ければ何もしない。
/// nvim プロセスが PATH に無い / 非 0 終了でもエラーにはせず、warn を stderr に
/// 流して Ok を返す (resilience)。
pub async fn build_helptags(
    plugin_scripts: &[PluginScripts],
    merged_dir: &Path,
) -> anyhow::Result<HelptagsReport> {
    let targets = collect_helptag_targets(plugin_scripts, merged_dir);
    if targets.is_empty() {
        return Ok(HelptagsReport {
            target_count: 0,
            ran: false,
            exit_code: None,
        });
    }

    let script = build_helptags_script(&targets);
    let tmp_path = std::env::temp_dir().join(format!(
        "rvpm-helptags-{}-{}.vim",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&tmp_path, &script)?;

    // tmp_path に空白や Vim 特殊文字が混じっても壊れないよう fnameescape() で wrap。
    // Windows の TEMP が "C:\Users\foo bar\..." 形式になるケースを想定。
    let escaped_path = tmp_path
        .display()
        .to_string()
        .replace('\\', "/")
        .replace('\'', "''");
    let source_arg = format!("execute 'source ' . fnameescape('{}')", escaped_path);
    let result = tokio::process::Command::new("nvim")
        .args(["--headless", "--clean", "-c", &source_arg, "-c", "qa!"])
        .output()
        .await;

    let _ = std::fs::remove_file(&tmp_path);

    match result {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stderr_trim = stderr.trim();
            if !stderr_trim.is_empty() {
                eprintln!("{}", stderr_trim);
            }
            let exit_code = out.status.code();
            Ok(HelptagsReport {
                target_count: targets.len(),
                ran: true,
                exit_code,
            })
        }
        Err(e) => {
            eprintln!(
                "\u{26a0} helptags skipped: could not run `nvim --headless` ({})",
                e
            );
            Ok(HelptagsReport {
                target_count: targets.len(),
                ran: false,
                exit_code: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_plugin(name: &str, path: &Path, lazy: bool, merge: bool) -> PluginScripts {
        let mut s = PluginScripts::for_test(name, path.to_str().unwrap());
        s.lazy = lazy;
        s.merge = merge;
        s
    }

    fn create_doc(root: &Path, plugin: &str) -> PathBuf {
        let plugin_dir = root.join(plugin);
        let doc = plugin_dir.join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("foo.txt"), b"*foo-tag*\n").unwrap();
        plugin_dir
    }

    #[test]
    fn test_collect_targets_includes_merged_doc_when_exists() {
        let tmp = TempDir::new().unwrap();
        let merged = tmp.path().join("merged");
        std::fs::create_dir_all(merged.join("doc")).unwrap();
        let targets = collect_helptag_targets(&[], &merged);
        assert_eq!(targets, vec![merged.join("doc")]);
    }

    #[test]
    fn test_collect_targets_skips_merged_when_doc_missing() {
        let tmp = TempDir::new().unwrap();
        let merged = tmp.path().join("merged");
        std::fs::create_dir_all(&merged).unwrap();
        let targets = collect_helptag_targets(&[], &merged);
        assert!(targets.is_empty());
    }

    #[test]
    fn test_collect_targets_skips_eager_merge_plugin() {
        // eager + merge=true は merged/doc/ 経由で処理される前提なので個別に含めない
        let tmp = TempDir::new().unwrap();
        let plugin_dir = create_doc(tmp.path(), "plug1");
        let ps = make_plugin("plug1", &plugin_dir, false, true);
        let merged = tmp.path().join("merged");
        std::fs::create_dir_all(&merged).unwrap();
        let targets = collect_helptag_targets(&[ps], &merged);
        assert!(targets.is_empty());
    }

    #[test]
    fn test_collect_targets_includes_lazy_plugin_even_when_merge_true() {
        // lazy プラグインは merge=true でも merged に入らない (trigger 前に rtp 漏れを避けるため)
        let tmp = TempDir::new().unwrap();
        let plugin_dir = create_doc(tmp.path(), "plug2");
        let ps = make_plugin("plug2", &plugin_dir, true, true);
        let merged = tmp.path().join("merged");
        std::fs::create_dir_all(&merged).unwrap();
        let targets = collect_helptag_targets(&[ps], &merged);
        assert_eq!(targets, vec![plugin_dir.join("doc")]);
    }

    #[test]
    fn test_collect_targets_includes_eager_non_merge_plugin() {
        let tmp = TempDir::new().unwrap();
        let plugin_dir = create_doc(tmp.path(), "plug3");
        let ps = make_plugin("plug3", &plugin_dir, false, false);
        let merged = tmp.path().join("merged");
        std::fs::create_dir_all(&merged).unwrap();
        let targets = collect_helptag_targets(&[ps], &merged);
        assert_eq!(targets, vec![plugin_dir.join("doc")]);
    }

    #[test]
    fn test_collect_targets_skips_plugin_without_doc() {
        let tmp = TempDir::new().unwrap();
        let plugin_dir = tmp.path().join("no_doc");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let ps = make_plugin("no_doc", &plugin_dir, true, false);
        let merged = tmp.path().join("merged");
        std::fs::create_dir_all(&merged).unwrap();
        let targets = collect_helptag_targets(&[ps], &merged);
        assert!(targets.is_empty());
    }

    #[test]
    fn test_collect_targets_orders_merged_first_then_per_plugin() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("merged").join("doc")).unwrap();
        let lazy_plugin = create_doc(tmp.path(), "lazy_p");
        let non_merge = create_doc(tmp.path(), "non_merge");
        let scripts = vec![
            make_plugin("lazy_p", &lazy_plugin, true, true),
            make_plugin("non_merge", &non_merge, false, false),
        ];
        let merged = tmp.path().join("merged");
        let targets = collect_helptag_targets(&scripts, &merged);
        assert_eq!(
            targets,
            vec![
                merged.join("doc"),
                lazy_plugin.join("doc"),
                non_merge.join("doc"),
            ]
        );
    }

    #[test]
    fn test_build_script_uses_fnameescape_and_try_catch() {
        let path = PathBuf::from("/tmp/some plugin/doc");
        let script = build_helptags_script(&[path]);
        assert!(script.contains("fnameescape('/tmp/some plugin/doc')"));
        assert!(script.contains("try |"));
        assert!(script.contains("| endtry"));
    }

    #[test]
    fn test_build_script_escapes_single_quote_in_path() {
        let path = PathBuf::from("/tmp/it's/doc");
        let script = build_helptags_script(&[path]);
        // '→'' で escape されていること
        assert!(script.contains("'/tmp/it''s/doc'"));
    }

    #[test]
    fn test_build_script_normalizes_backslashes_to_forward() {
        let path = PathBuf::from(r"C:\Users\foo\plugin\doc");
        let script = build_helptags_script(&[path]);
        assert!(script.contains("'C:/Users/foo/plugin/doc'"));
        assert!(!script.contains(r"\Users"));
    }

    #[tokio::test]
    async fn test_build_helptags_skips_when_no_targets() {
        let tmp = TempDir::new().unwrap();
        let merged = tmp.path().join("merged");
        std::fs::create_dir_all(&merged).unwrap();
        let report = build_helptags(&[], &merged).await.unwrap();
        assert_eq!(report.target_count, 0);
        assert!(!report.ran);
    }
}
