use anyhow::Result;
use std::path::Path;

pub fn junction_or_symlink(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        if dst.is_dir() {
            let _ = std::fs::remove_dir_all(dst);
        } else {
            std::fs::remove_file(dst)?;
        }
    }

    if src.is_dir() {
        #[cfg(windows)]
        {
            junction::create(src, dst)?;
        }
        #[cfg(not(windows))]
        {
            std::os::unix::fs::symlink(src, dst)?;
        }
    } else {
        // ファイルの場合はコピー（Windows での権限問題を避けるため）
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// 指定したプラグインの全ディレクトリ・ファイルを merged ディレクトリにマージ。
/// `.git` 等の隠しディレクトリは除外し、それ以外の全エントリをリンクする。
/// ディレクトリの場合はサブエントリ単位でリンク (衝突時は上書き)。
/// ファイルの場合はコピー。
pub fn merge_plugin(src: &Path, dst_root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // 隠しファイル・ディレクトリ (.git, .github, etc) は除外
        if name_str.starts_with('.') {
            continue;
        }

        let entry_src = entry.path();

        if entry_src.is_dir() {
            // ディレクトリ: merged/<dir>/ を作成して中身をリンク
            let target_dst = dst_root.join(&name);
            if !target_dst.exists() {
                std::fs::create_dir_all(&target_dst)?;
            }
            for sub in std::fs::read_dir(&entry_src)? {
                let sub = sub?;
                let sub_src = sub.path();
                let sub_dst = target_dst.join(sub.file_name());
                junction_or_symlink(&sub_src, &sub_dst)?;
            }
        } else {
            // ファイル: そのままコピー
            let file_dst = dst_root.join(&name);
            std::fs::copy(&entry_src, &file_dst)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_merge_plugins() {
        let root = tempdir().unwrap();
        let merged = root.path().join("merged");

        // Plugin A
        let plugin_a = root.path().join("plugin_a");
        fs::create_dir_all(plugin_a.join("lua/plugin_a")).unwrap();
        fs::write(plugin_a.join("lua/plugin_a/init.lua"), "print('a')").unwrap();

        // Plugin B
        let plugin_b = root.path().join("plugin_b");
        fs::create_dir_all(plugin_b.join("plugin")).unwrap();
        fs::write(plugin_b.join("plugin/b.vim"), "echo 'b'").unwrap();

        // マージ実行
        merge_plugin(&plugin_a, &merged).unwrap();
        merge_plugin(&plugin_b, &merged).unwrap();

        // 期待される構造の確認
        assert!(merged.join("lua/plugin_a/init.lua").exists());
        assert!(merged.join("plugin/b.vim").exists());
    }

    #[test]
    fn test_junction_creation() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("hello.txt"), "hello").unwrap();

        // 実行
        junction_or_symlink(&src, &dst).unwrap();

        // リンク先でファイルが読めるか確認
        assert!(dst.join("hello.txt").exists());
        let content = fs::read_to_string(dst.join("hello.txt")).unwrap();
        assert_eq!(content, "hello");
    }
}
