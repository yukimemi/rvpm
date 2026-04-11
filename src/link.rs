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

/// 指定したプラグインの中身（lua, plugin, after 等）を merged ディレクトリにマージ
pub fn merge_plugin(src: &Path, dst_root: &Path) -> Result<()> {
    let targets = [
        "lua", "plugin", "after", "autoload", "doc", "colors", "queries", "syntax",
    ];

    for target in targets {
        let target_src = src.join(target);
        if target_src.exists() {
            let target_dst = dst_root.join(target);

            // ターゲット（例: merged/lua）がまだ存在しない場合は作成
            if !target_dst.exists() {
                std::fs::create_dir_all(&target_dst)?;
            }

            // 中身を再帰的にリンク... ではなく、まずは単純にサブディレクトリ単位でリンク。
            // 同じプラグインが同じディレクトリ（例: plugin/a.vim）を持つ場合は上書き。
            // ここでは簡易的に、各プラグインのサブディレクトリ（例: lua/plugin_a）を
            // ターゲット（merged/lua/plugin_a）としてリンクする。
            for entry in std::fs::read_dir(&target_src)? {
                let entry = entry?;
                let entry_src = entry.path();
                let entry_dst = target_dst.join(entry.file_name());
                junction_or_symlink(&entry_src, &entry_dst)?;
            }
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
