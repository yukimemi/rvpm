use anyhow::Result;
use gix::bstr::BString;
use std::path::Path;

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
    Error(String),
}

impl<'a> Repo<'a> {
    pub fn new(url: &'a str, dst: &'a Path, rev: Option<&'a str>) -> Self {
        Self { url, dst, rev }
    }

    pub async fn sync(&self) -> Result<()> {
        let url = resolve_url(self.url);
        let dst = self.dst.to_path_buf();
        let rev = self.rev.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || sync_impl(&url, &dst, rev.as_deref()))
            .await
            .map_err(|e| anyhow::anyhow!("sync task panicked: {}", e))?
    }

    pub async fn update(&self) -> Result<()> {
        let url = resolve_url(self.url);
        let dst = self.dst.to_path_buf();
        let rev = self.rev.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || update_impl(&url, &dst, rev.as_deref()))
            .await
            .map_err(|e| anyhow::anyhow!("update task panicked: {}", e))?
    }

    pub async fn get_status(&self) -> RepoStatus {
        let dst = self.dst.to_path_buf();
        let rev = self.rev.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || get_status_impl(&dst, rev.as_deref()))
            .await
            .unwrap_or(RepoStatus::Error("status check panicked".to_string()))
    }
}

fn resolve_url(url: &str) -> String {
    if !url.contains("://") && !url.contains('@') && !url.contains(":\\") && !url.starts_with('/') {
        format!("https://github.com/{}", url)
    } else {
        url.to_string()
    }
}

// ======================================================
// clone / fetch — gix で in-process 実行
// checkout — gix の checkout API は複雑なため git コマンドにフォールバック
// status — gix で in-process 実行 (プロセス fork なし)
// ======================================================

fn sync_impl(url: &str, dst: &Path, rev: Option<&str>) -> Result<()> {
    if dst.exists() {
        fetch_impl(dst)?;
        if let Some(rev) = rev {
            gix_checkout(dst, rev)?;
        } else {
            gix_reset_to_remote(dst)?;
        }
    } else {
        clone_impl(url, dst)?;
        if let Some(rev) = rev {
            gix_checkout(dst, rev)?;
        }
    }
    Ok(())
}

fn update_impl(_url: &str, dst: &Path, rev: Option<&str>) -> Result<()> {
    if !dst.exists() {
        anyhow::bail!("Plugin not installed: {}", dst.display());
    }
    fetch_impl(dst)?;
    if let Some(rev) = rev {
        gix_checkout(dst, rev)?;
    } else {
        gix_reset_to_remote(dst)?;
    }
    Ok(())
}

fn clone_impl(url: &str, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // shallow clone (depth 1) で高速化
    let (mut _checkout, _outcome) = gix::prepare_clone(url, dst)?
        .with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(
            std::num::NonZeroU32::new(1).unwrap(),
        ))
        .fetch_then_checkout(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(dst);
            anyhow::anyhow!("git clone failed: {}", e)
        })?;

    _checkout
        .main_worktree(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(dst);
            anyhow::anyhow!("checkout failed: {}", e)
        })?;

    Ok(())
}

fn fetch_impl(dst: &Path) -> Result<()> {
    let repo = gix::open(dst)?;
    let remote = repo
        .find_default_remote(gix::remote::Direction::Fetch)
        .ok_or_else(|| anyhow::anyhow!("no remote configured"))??;

    remote
        .connect(gix::remote::Direction::Fetch)?
        .prepare_fetch(gix::progress::Discard, Default::default())?
        .with_shallow(gix::remote::fetch::Shallow::Deepen(1))
        .receive(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)?;

    Ok(())
}

/// gix で特定の rev に checkout。branch の場合は branch を維持。
fn gix_checkout(dst: &Path, rev: &str) -> Result<()> {
    let repo = gix::open(dst)?;
    let target = repo
        .rev_parse_single(rev)
        .map_err(|_| anyhow::anyhow!("rev '{}' not found", rev))?;
    let commit_id = target.detach();

    // rev が local branch を指す場合は symbolic HEAD を設定
    let branch_ref = format!("refs/heads/{}", rev);
    if repo.find_reference(&branch_ref).is_ok() {
        // HEAD を symbolic ref にする (直接ファイル書き込み)
        let head_path = repo.git_dir().join("HEAD");
        std::fs::write(&head_path, format!("ref: {}\n", branch_ref))?;
        // branch ref を更新
        repo.reference(
            branch_ref.as_str(),
            commit_id,
            gix::refs::transaction::PreviousValue::Any,
            BString::from(format!("rvpm: checkout branch {}", rev)),
        )?;
    } else {
        // tag/hash の場合は detached HEAD
        repo.reference(
            "HEAD",
            commit_id,
            gix::refs::transaction::PreviousValue::Any,
            BString::from(format!("rvpm: checkout {}", rev)),
        )?;
    }

    gix_checkout_head(&repo)?;
    Ok(())
}

/// fetch 後に working tree を remote の最新に更新 (git reset --hard 相当)。
fn gix_reset_to_remote(dst: &Path) -> Result<()> {
    let repo = gix::open(dst)?;

    // remote 名を動的に取得 (通常は "origin")
    let remote_name = repo
        .find_default_remote(gix::remote::Direction::Fetch)
        .and_then(|r| r.ok())
        .and_then(|r| r.name().map(|n| n.as_bstr().to_string()))
        .unwrap_or_else(|| "origin".to_string());

    // remote tracking branch からターゲット commit を取得
    let target_id = {
        let head_name = repo.head_name()?;
        let tracking_ref = if let Some(ref name) = head_name {
            // refs/heads/master → refs/remotes/<remote>/master
            let branch = name.as_bstr().to_string();
            let tracking = branch.replace("refs/heads/", &format!("refs/remotes/{}/", remote_name));
            repo.find_reference(&tracking).ok()
        } else {
            None
        };

        if let Some(mut tr) = tracking_ref {
            tr.peel_to_id_in_place()?.detach()
        } else {
            // フォールバック: <remote>/HEAD
            let remote_head = format!("refs/remotes/{}/HEAD", remote_name);
            if let Ok(mut r) = repo.find_reference(&remote_head) {
                r.peel_to_id_in_place()?.detach()
            } else {
                return Ok(());
            }
        }
    };

    // ローカル branch を更新
    if let Some(head_name) = repo.head_name()? {
        repo.reference(
            head_name.as_ref(),
            target_id,
            gix::refs::transaction::PreviousValue::Any,
            BString::from("rvpm: fast-forward"),
        )?;
    }

    // worktree を更新
    gix_checkout_head(&repo)?;
    Ok(())
}

/// HEAD の tree を worktree に展開 (gix_worktree_state::checkout)。
fn gix_checkout_head(repo: &gix::Repository) -> Result<()> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("bare repository"))?;

    let head = repo.head_commit()?;
    let tree_id = head.tree_id()?;

    let co_opts =
        repo.checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)?;
    let index = gix::index::State::from_tree(&tree_id, &repo.objects, Default::default())
        .map_err(|e| anyhow::anyhow!("index from tree: {}", e))?;
    let mut index_file = gix::index::File::from_state(index, repo.index_path());

    let opts = gix::worktree::state::checkout::Options {
        destination_is_initially_empty: false,
        overwrite_existing: true,
        ..co_opts
    };

    let progress = gix::progress::Discard;
    gix::worktree::state::checkout(
        &mut index_file,
        workdir,
        repo.objects.clone().into_arc()?,
        &progress,
        &progress,
        &gix::interrupt::IS_INTERRUPTED,
        opts,
    )
    .map_err(|e| anyhow::anyhow!("checkout failed: {}", e))?;

    index_file
        .write(Default::default())
        .map_err(|e| anyhow::anyhow!("write index: {}", e))?;

    Ok(())
}

/// gix を使ったプロセス fork なしのステータスチェック。
fn get_status_impl(dst: &Path, rev: Option<&str>) -> RepoStatus {
    if !dst.exists() {
        return RepoStatus::NotInstalled;
    }

    let repo = match gix::open(dst) {
        Ok(r) => r,
        Err(_) => return RepoStatus::Error("Failed to open git repo".to_string()),
    };

    // ワーキングツリーの変更を検出
    match repo.is_dirty() {
        Ok(true) => return RepoStatus::Modified,
        Ok(false) => {}
        Err(_) => return RepoStatus::Clean, // フォールバック
    }

    // rev が指定されている場合、ローカルに存在するか確認
    if let Some(rev) = rev {
        match repo.rev_parse_single(rev) {
            Ok(_) => {}
            Err(_) => return RepoStatus::Error(format!("rev '{}' not found in local repo", rev)),
        }
    }

    RepoStatus::Clean
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use tokio::process::Command;

    fn git_cmd(dir: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", dir.join(".gitconfig-test"))
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com");
        cmd
    }

    #[tokio::test]
    async fn test_get_status_not_installed() {
        let root = tempdir().unwrap();
        let dst = root.path().join("nonexistent");
        let repo = Repo::new("dummy", &dst, None);
        assert_eq!(repo.get_status().await, RepoStatus::NotInstalled);
    }

    #[tokio::test]
    async fn test_get_status_clean() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        fs::create_dir_all(&src).unwrap();
        git_cmd(&src).args(["init"]).output().await.unwrap();
        fs::write(src.join("hello.txt"), "hello").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();

        let repo = Repo::new(src.to_str().unwrap(), &src, None);
        assert_eq!(repo.get_status().await, RepoStatus::Clean);
    }

    #[tokio::test]
    async fn test_get_status_modified() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        fs::create_dir_all(&src).unwrap();
        git_cmd(&src).args(["init"]).output().await.unwrap();
        fs::write(src.join("hello.txt"), "hello").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();

        fs::write(src.join("hello.txt"), "modified").unwrap();
        let repo = Repo::new(src.to_str().unwrap(), &src, None);
        assert_eq!(repo.get_status().await, RepoStatus::Modified);
    }

    #[tokio::test]
    async fn test_get_status_errors_on_invalid_rev() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        fs::create_dir_all(&src).unwrap();
        git_cmd(&src).args(["init"]).output().await.unwrap();
        fs::write(src.join("hello.txt"), "hello").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();

        let repo = Repo::new(src.to_str().unwrap(), &src, Some("nonexistent-rev"));
        let status = repo.get_status().await;
        assert!(matches!(status, RepoStatus::Error(_)));
    }

    #[tokio::test]
    async fn test_update_fails_when_not_installed() {
        let root = tempdir().unwrap();
        let dst = root.path().join("nonexistent");
        let repo = Repo::new("dummy/repo", &dst, None);
        let result = repo.update().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not installed"));
    }

    #[tokio::test]
    async fn test_resolve_url_adds_github_prefix() {
        assert_eq!(resolve_url("owner/repo"), "https://github.com/owner/repo");
        assert_eq!(
            resolve_url("https://github.com/owner/repo"),
            "https://github.com/owner/repo"
        );
    }

    #[tokio::test]
    async fn test_sync_clones_new_repo() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        // ローカル bare repo を作成
        fs::create_dir_all(&src).unwrap();
        git_cmd(&src).args(["init"]).output().await.unwrap();
        fs::write(src.join("hello.txt"), "hello").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        assert!(dst.join("hello.txt").exists());
        let content = fs::read_to_string(dst.join("hello.txt")).unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn test_sync_updates_existing_repo() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_cmd(&src).args(["init"]).output().await.unwrap();
        fs::write(src.join("hello.txt"), "hello").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        // src を更新
        fs::write(src.join("hello.txt"), "updated").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "update"])
            .output()
            .await
            .unwrap();

        // 再 sync
        repo.sync().await.unwrap();

        let content = fs::read_to_string(dst.join("hello.txt")).unwrap();
        assert_eq!(content, "updated");
    }
}
