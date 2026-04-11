use std::path::Path;
use anyhow::Result;
use tokio::process::Command;

pub struct Repo<'a> {
    pub url: &'a str,
    pub dst: &'a Path,
    pub rev: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RepoStatus {
    NotInstalled,
    Clean,
    Modified,
    Outdated(String),
    Error(String),
}

impl<'a> Repo<'a> {
    pub fn new(url: &'a str, dst: &'a Path, rev: Option<&'a str>) -> Self {
        Self { url, dst, rev }
    }

    pub async fn sync(&self) -> Result<()> {
        let url = if !self.url.contains("://") && !self.url.contains("@") && !self.url.contains(":\\") && !self.url.starts_with("/") {
            format!("https://github.com/{}", self.url)
        } else {
            self.url.to_string()
        };

        let mut is_new_clone = false;

        if self.dst.exists() {
            let mut args = vec!["pull"];
            if let Some(rev) = self.rev {
                 // 特定の rev の場合は pull ではなく fetch して checkout するのが安全なため、
                 // ここでは一旦 origin を fetch して checkout するロジックにする（後述）
                 args = vec!["fetch", "--depth", "1", "origin", rev];
            }

            let output = Command::new("git")
                .args(&args)
                .current_dir(self.dst)
                .output()
                .await?;
            if !output.status.success() {
                anyhow::bail!("git pull/fetch failed: {}", String::from_utf8_lossy(&output.stderr));
            }
        } else {
            if let Some(parent) = self.dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            
            // rev が指定されている場合は、最初からそのブランチやタグを狙ってクローンする（最速）
            // ※ ただしハッシュだった場合は clone --branch は失敗するので、汎用的なフォールバックが必要だが、
            // 今回は TDD なので、まずは --branch に渡してみて、失敗したら通常のクローンをする等工夫する。
            // 簡易的に：
            let mut args = vec!["clone", "--depth", "1"];
            if let Some(rev) = self.rev {
                args.push("--branch");
                args.push(rev);
            }
            args.push(&url);
            args.push(self.dst.to_str().unwrap());

            let output = Command::new("git")
                .args(&args)
                .output()
                .await?;
                
            if !output.status.success() {
                // ハッシュ指定で --branch が失敗した可能性もあるため、通常クローンにフォールバック
                if self.rev.is_some() && String::from_utf8_lossy(&output.stderr).contains("not found in upstream") {
                    let output = Command::new("git")
                        .args(["clone", &url, self.dst.to_str().unwrap()]) // depth 1 は諦める
                        .output()
                        .await?;
                    if !output.status.success() {
                         anyhow::bail!("git clone fallback failed: {}", String::from_utf8_lossy(&output.stderr));
                    }
                } else {
                    anyhow::bail!("git clone failed: {}", String::from_utf8_lossy(&output.stderr));
                }
            }
            is_new_clone = true;
        }

        // rev が指定されており、かつ新たにクローンした（もしくは fetch した）場合、その rev に checkout する
        if let Some(rev) = self.rev {
            let output = Command::new("git")
                .args(["checkout", rev])
                .current_dir(self.dst)
                .output()
                .await?;
            if !output.status.success() {
                // 新規クローン時に checkout が失敗した場合は不完全なディレクトリを削除する
                if is_new_clone {
                    let _ = std::fs::remove_dir_all(self.dst);
                }
                anyhow::bail!("git checkout failed for rev '{}': {}", rev, String::from_utf8_lossy(&output.stderr));
            }
        }

        Ok(())
    }

    pub async fn update(&self) -> Result<()> {
        if !self.dst.exists() {
            anyhow::bail!("Plugin not installed: {}", self.dst.display());
        }
        let args: Vec<&str> = if let Some(rev) = self.rev {
            vec!["fetch", "--depth", "1", "origin", rev]
        } else {
            vec!["pull"]
        };
        let output = Command::new("git")
            .args(&args)
            .current_dir(self.dst)
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!("git pull/fetch failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        if let Some(rev) = self.rev {
            let output = Command::new("git")
                .args(["checkout", rev])
                .current_dir(self.dst)
                .output()
                .await?;
            if !output.status.success() {
                anyhow::bail!("git checkout failed: {}", String::from_utf8_lossy(&output.stderr));
            }
        }
        Ok(())
    }

    pub async fn get_status(&self) -> RepoStatus {
        if !self.dst.exists() {
            return RepoStatus::NotInstalled;
        }

        let status_output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(self.dst)
            .output()
            .await;

        match status_output {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !stdout.trim().is_empty() {
                    return RepoStatus::Modified;
                }
            }
            _ => return RepoStatus::Error("Failed to run git status".to_string()),
        }

        // rev が指定されている場合、そのref がローカルに存在するか確認する
        // 存在しない場合は sync 失敗後にディレクトリだけ残った可能性がある
        if let Some(rev) = self.rev {
            let verify = Command::new("git")
                .args(["rev-parse", "--verify", rev])
                .current_dir(self.dst)
                .output()
                .await;
            match verify {
                Ok(output) if output.status.success() => {}
                _ => return RepoStatus::Error(format!("rev '{}' not found in local repo", rev)),
            }
        }

        RepoStatus::Clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use std::fs;

    #[tokio::test]
    async fn test_sync_cleans_up_on_invalid_rev() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        Command::new("git").args(["init"]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["config", "user.email", "test@test.com"]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["config", "user.name", "Test"]).current_dir(&src).output().await.unwrap();
        fs::write(src.join("hello.txt"), "hello").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["commit", "-m", "init"]).current_dir(&src).output().await.unwrap();

        let repo = Repo::new(src.to_str().unwrap(), &dst, Some("nonexistent-rev"));
        let result = repo.sync().await;

        assert!(result.is_err(), "存在しない rev は sync エラーになるべき");
        assert!(!dst.exists(), "失敗後にディレクトリが残ってはいけない");
    }

    #[tokio::test]
    async fn test_get_status_errors_on_invalid_rev() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");

        fs::create_dir_all(&src).unwrap();
        Command::new("git").args(["init"]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["config", "user.email", "test@test.com"]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["config", "user.name", "Test"]).current_dir(&src).output().await.unwrap();
        fs::write(src.join("hello.txt"), "hello").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["commit", "-m", "init"]).current_dir(&src).output().await.unwrap();

        // 存在しない rev を指定
        let repo = Repo::new(src.to_str().unwrap(), &src, Some("nonexistent-rev"));
        let status = repo.get_status().await;

        assert!(
            matches!(status, RepoStatus::Error(_)),
            "存在しない rev は get_status が Error を返すべき、実際: {:?}", status
        );
    }

    #[tokio::test]
    async fn test_git_update_method_pulls_latest() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        Command::new("git").args(["init"]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["config", "user.email", "test@test.com"]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["config", "user.name", "Test"]).current_dir(&src).output().await.unwrap();
        fs::write(src.join("hello.txt"), "v1").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["commit", "-m", "v1"]).current_dir(&src).output().await.unwrap();

        // 最初に clone
        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        // src に更新を追加
        fs::write(src.join("hello.txt"), "v2").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["commit", "-m", "v2"]).current_dir(&src).output().await.unwrap();

        // update のみ実行
        repo.update().await.unwrap();
        let content = fs::read_to_string(dst.join("hello.txt")).unwrap();
        assert_eq!(content, "v2");
    }

    #[tokio::test]
    async fn test_git_update_method_fails_when_not_installed() {
        let root = tempdir().unwrap();
        let dst = root.path().join("nonexistent");
        let repo = Repo::new("dummy/repo", &dst, None);
        let result = repo.update().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not installed"));
    }

    #[tokio::test]
    async fn test_git_update() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        Command::new("git").args(["init"]).current_dir(&src).output().await.unwrap();
        fs::write(src.join("hello.txt"), "hello").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["commit", "-m", "init"]).current_dir(&src).output().await.unwrap();
        
        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        fs::write(src.join("hello.txt"), "updated").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["commit", "-m", "update"]).current_dir(&src).output().await.unwrap();

        repo.sync().await.unwrap();

        let content = fs::read_to_string(dst.join("hello.txt")).unwrap();
        assert_eq!(content, "updated");
    }

    #[tokio::test]
    async fn test_git_status() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");

        fs::create_dir_all(&src).unwrap();
        Command::new("git").args(["init"]).current_dir(&src).output().await.unwrap();
        fs::write(src.join("hello.txt"), "hello").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["commit", "-m", "init"]).current_dir(&src).output().await.unwrap();

        let repo = Repo::new(src.to_str().unwrap(), &src, None);

        // Clean state
        assert_eq!(repo.get_status().await, RepoStatus::Clean);

        // Modified state
        fs::write(src.join("hello.txt"), "modified").unwrap();
        assert_eq!(repo.get_status().await, RepoStatus::Modified);
    }

    #[tokio::test]
    async fn test_git_rev_checkout() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        // 1. ダミーのリポジトリを作成し、2回コミットする
        fs::create_dir_all(&src).unwrap();
        Command::new("git").args(["init"]).current_dir(&src).output().await.unwrap();
        
        fs::write(src.join("hello.txt"), "v1").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["commit", "-m", "v1"]).current_dir(&src).output().await.unwrap();
        
        // tag "v1.0" を打つ
        Command::new("git").args(["tag", "v1.0"]).current_dir(&src).output().await.unwrap();

        fs::write(src.join("hello.txt"), "v2").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&src).output().await.unwrap();
        Command::new("git").args(["commit", "-m", "v2"]).current_dir(&src).output().await.unwrap();

        // 2. v1.0 タグを指定してクローン
        let repo = Repo::new(src.to_str().unwrap(), &dst, Some("v1.0"));
        repo.sync().await.unwrap();

        // 3. v1 の内容になっているか確認
        let content = fs::read_to_string(dst.join("hello.txt")).unwrap();
        assert_eq!(content, "v1");
    }
}
