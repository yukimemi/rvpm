use std::path::Path;
use anyhow::Result;
use tokio::process::Command;

pub struct Repo<'a> {
    pub url: &'a str,
    pub dst: &'a Path,
}

impl<'a> Repo<'a> {
    pub fn new(url: &'a str, dst: &'a Path) -> Self {
        Self { url, dst }
    }

    pub async fn sync(&self) -> Result<()> {
        let url = if !self.url.contains("://") && !self.url.contains("@") && !self.url.contains(":\\") && !self.url.starts_with("/") {
            format!("https://github.com/{}", self.url)
        } else {
            self.url.to_string()
        };

        if self.dst.exists() {
            let output = Command::new("git")
                .args(["pull"])
                .current_dir(self.dst)
                .output()
                .await?;
            if !output.status.success() {
                anyhow::bail!("git pull failed: {}", String::from_utf8_lossy(&output.stderr));
            }
        } else {
            // 親ディレクトリを作成
            if let Some(parent) = self.dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let output = Command::new("git")
                .args(["clone", "--depth", "1", &url, &self.dst.to_string_lossy()])
                .output()
                .await?;
            if !output.status.success() {
                anyhow::bail!("git clone failed: {}", String::from_utf8_lossy(&output.stderr));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use std::fs;

    #[tokio::test]
    async fn test_git_update() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        // 1. ダミーのリポジトリを作成してクローン
        fs::create_dir_all(&src).unwrap();
        Command::new("git").args(["init"]).current_dir(&src).output().await.unwrap();
        fs::write(src.join("hello.txt"), "hello").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["commit", "-m", "init"]).current_dir(&src).output().await.unwrap();
        
        let repo = Repo::new(src.to_str().unwrap(), &dst);
        repo.sync().await.unwrap();

        // 2. ソースを更新
        fs::write(src.join("hello.txt"), "updated").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["commit", "-m", "update"]).current_dir(&src).output().await.unwrap();

        // 3. sync で更新を反映
        repo.sync().await.unwrap();

        // 4. 内容が更新されているか確認
        let content = fs::read_to_string(dst.join("hello.txt")).unwrap();
        assert_eq!(content, "updated");
    }
}
