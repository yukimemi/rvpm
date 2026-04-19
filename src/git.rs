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

/// `Repo::sync` / `Repo::update` の差分情報。`rvpm log` の永続化用。
///
/// `from = None` は新規 clone を意味する (commit walk もしないので subjects 等は空)。
/// `from == to` (no-op の sync / update) の場合、呼び出し側は `Option<GitChange>::None`
/// を受け取る (Repo 側で「変更なし」を判別して丸める)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitChange {
    pub from: Option<String>,
    pub to: String,
    pub subjects: Vec<String>,
    pub breaking_subjects: Vec<String>,
    pub doc_files_changed: Vec<String>,
}

impl<'a> Repo<'a> {
    pub fn new(url: &'a str, dst: &'a Path, rev: Option<&'a str>) -> Self {
        Self { url, dst, rev }
    }

    /// clone 済みなら fetch + checkout、未 clone なら shallow clone。
    /// `Option<GitChange>` で差分を返す。HEAD が動かなかった場合は `None`。
    pub async fn sync(&self) -> Result<Option<GitChange>> {
        let url = resolve_url(self.url);
        let dst = self.dst.to_path_buf();
        let rev = self.rev.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || sync_impl(&url, &dst, rev.as_deref()))
            .await
            .map_err(|e| anyhow::anyhow!("sync task panicked: {}", e))?
    }

    /// 既存 clone のみ受け付けて pull する。`Option<GitChange>` で差分を返す。
    /// HEAD が動かなかった場合は `None`。
    pub async fn update(&self) -> Result<Option<GitChange>> {
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

    /// 現在 checkout 中の HEAD commit hash を返す。
    /// lockfile 書き込み時に "no-op sync でも現在の commit を記録する" ために使う
    /// (`sync()` の `GitChange` は HEAD が動いた時しか返されないため)。
    pub async fn head_commit(&self) -> Result<String> {
        let dst = self.dst.to_path_buf();
        tokio::task::spawn_blocking(move || read_head(&dst))
            .await
            .map_err(|e| anyhow::anyhow!("head_commit task panicked: {}", e))?
    }
}

/// owner/repo 形式のショートハンドを GitHub URL に変換。
/// ローカルパス (./  ../  ~/  絶対パス等) はそのまま返す。
fn resolve_url(url: &str) -> String {
    // 明らかに URL やパスの場合はそのまま
    if url.contains("://")
        || url.contains('@')
        || url.starts_with('/')
        || url.starts_with('~')
        || url.starts_with('.')
        || url.starts_with('\\')
        || (url.len() >= 2 && url.as_bytes()[1] == b':')
    // C:\ 等
    {
        return url.to_string();
    }
    // owner/repo 形式: exactly one slash, no special chars
    if url.matches('/').count() == 1 && !url.contains(' ') {
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

fn sync_impl(url: &str, dst: &Path, rev: Option<&str>) -> Result<Option<GitChange>> {
    if dst.exists() {
        let before = read_head(dst).ok();
        fetch_impl(dst)?;
        if let Some(rev) = rev {
            gix_checkout(dst, rev)?;
        } else {
            gix_reset_to_remote(dst)?;
        }
        let after = read_head(dst)?;
        Ok(build_change(dst, before, after))
    } else {
        clone_impl(url, dst)?;
        if let Some(rev) = rev {
            gix_checkout(dst, rev)?;
        }
        let after = read_head(dst)?;
        // 新規 clone は from = None。subjects は空のまま。
        Ok(Some(GitChange {
            from: None,
            to: after,
            subjects: Vec::new(),
            breaking_subjects: Vec::new(),
            doc_files_changed: Vec::new(),
        }))
    }
}

fn update_impl(_url: &str, dst: &Path, rev: Option<&str>) -> Result<Option<GitChange>> {
    if !dst.exists() {
        anyhow::bail!("Plugin not installed: {}", dst.display());
    }
    let before = read_head(dst).ok();
    fetch_impl(dst)?;
    if let Some(rev) = rev {
        gix_checkout(dst, rev)?;
    } else {
        gix_reset_to_remote(dst)?;
    }
    let after = read_head(dst)?;
    Ok(build_change(dst, before, after))
}

/// HEAD の commit hash を読み取る。failure は呼び出し側で None 化することもある。
fn read_head(dst: &Path) -> Result<String> {
    let repo = gix::open(dst)?;
    let head = repo.head_commit()?;
    Ok(head.id().to_string())
}

/// before/after の HEAD から `GitChange` を組み立てる。
/// before == after なら `None` (no-op の sync/update を caller が判別できるように)。
fn build_change(dst: &Path, before: Option<String>, after: String) -> Option<GitChange> {
    match before {
        Some(b) if b == after => None,
        Some(b) => {
            let (subjects, breaking) = collect_subjects_and_breaking(dst, &b, &after);
            let doc_files = doc_files_changed(dst, &b, &after);
            Some(GitChange {
                from: Some(b),
                to: after,
                subjects,
                breaking_subjects: breaking,
                doc_files_changed: doc_files,
            })
        }
        None => Some(GitChange {
            from: None,
            to: after,
            subjects: Vec::new(),
            breaking_subjects: Vec::new(),
            doc_files_changed: Vec::new(),
        }),
    }
}

/// `<from>..<to>` を gix で walk し、(subjects, breaking_subjects) を返す。
/// commit graph の取得や revparse に失敗した場合は空ベクタ (resilience: log は best-effort)。
fn collect_subjects_and_breaking(dst: &Path, from: &str, to: &str) -> (Vec<String>, Vec<String>) {
    let mut subjects = Vec::new();
    let mut breaking = Vec::new();

    let repo = match gix::open(dst) {
        Ok(r) => r,
        Err(_) => return (subjects, breaking),
    };
    let from_id = match repo.rev_parse_single(from) {
        Ok(id) => id.detach(),
        Err(_) => return (subjects, breaking),
    };
    let to_id = match repo.rev_parse_single(to) {
        Ok(id) => id.detach(),
        Err(_) => return (subjects, breaking),
    };

    // walk to → ... → from (exclude from itself)
    let walk = match repo.rev_walk([to_id]).with_hidden([from_id]).all() {
        Ok(w) => w,
        Err(_) => return (subjects, breaking),
    };

    // 上限: 長期未更新後の pull や branch 切り替えで履歴が膨大になっても
    // `update_log.json` を肥大化させないため、subjects は最大 100 commit に制限。
    // 100 を超えた場合は新しい順 100 件だけ残る (rev_walk は新しい順)。
    const SUBJECT_WALK_LIMIT: usize = 100;
    for info in walk.flatten().take(SUBJECT_WALK_LIMIT) {
        let commit = match info.object() {
            Ok(c) => c,
            Err(_) => continue,
        };
        // gix の message_raw_sloppy は subject + body 全部入りの bytes。
        // subject は最初の改行まで、body は残り。
        let message = commit.message_raw_sloppy().to_string();
        let (subject, body) = split_subject_body(&message);
        let subj_str = subject.trim().to_string();
        if subj_str.is_empty() {
            continue;
        }
        let is_break = crate::update_log::is_breaking(&subj_str, body);
        if is_break {
            breaking.push(subj_str.clone());
        }
        subjects.push(subj_str);
    }

    (subjects, breaking)
}

fn split_subject_body(msg: &str) -> (&str, &str) {
    if let Some(idx) = msg.find('\n') {
        (&msg[..idx], &msg[idx + 1..])
    } else {
        (msg, "")
    }
}

/// `<from>..<to>` で変更があった README/CHANGELOG/doc 系ファイルの相対パス一覧を返す。
/// `git diff --name-only` を spawn する (gix の diff API は複雑なため subprocess)。
/// `git` が PATH に無い / 失敗時は空 Vec (resilience)。
fn doc_files_changed(dst: &Path, from: &str, to: &str) -> Vec<String> {
    let output = match std::process::Command::new("git")
        .arg("-C")
        .arg(dst)
        .args([
            "diff",
            "--name-only",
            &format!("{}..{}", from, to),
            "--",
            "README*",
            "readme*",
            "Readme*",
            "CHANGELOG*",
            "changelog*",
            "Changelog*",
            "doc/",
        ])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files: Vec<String> = stdout
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    files.sort();
    files.dedup();
    files
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
            tr.peel_to_id()?.detach()
        } else {
            // フォールバック: <remote>/HEAD
            let remote_head = format!("refs/remotes/{}/HEAD", remote_name);
            if let Ok(mut r) = repo.find_reference(&remote_head) {
                r.peel_to_id()?.detach()
            } else {
                return Ok(());
            }
        }
    };

    // ローカル branch を更新 (detached HEAD の場合は HEAD 直接更新)
    if let Some(head_name) = repo.head_name()? {
        repo.reference(
            head_name.as_ref(),
            target_id,
            gix::refs::transaction::PreviousValue::Any,
            BString::from("rvpm: fast-forward"),
        )?;
    } else {
        repo.reference(
            "HEAD",
            target_id,
            gix::refs::transaction::PreviousValue::Any,
            BString::from("rvpm: fast-forward detached"),
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
        Err(e) => return RepoStatus::Error(format!("status check failed: {}", e)),
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
        let change = repo.sync().await.unwrap();

        assert!(dst.join("hello.txt").exists());
        let content = fs::read_to_string(dst.join("hello.txt")).unwrap();
        assert_eq!(content, "hello");

        // 新規 clone は from = None で GitChange::Some を返す
        let c = change.expect("new clone should produce a GitChange");
        assert!(c.from.is_none());
        assert!(!c.to.is_empty());
        assert!(c.subjects.is_empty());
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
        let initial = repo.sync().await.unwrap();
        assert!(initial.is_some(), "first sync = clone produces a change");

        // 同じ HEAD で再 sync → no-op (None)
        let noop = repo.sync().await.unwrap();
        assert!(noop.is_none(), "no-op sync should yield None");

        // src を更新
        fs::write(src.join("hello.txt"), "updated").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "update"])
            .output()
            .await
            .unwrap();

        // 再 sync で差分発生
        let updated = repo.sync().await.unwrap().expect("HEAD moved");
        assert!(updated.from.is_some(), "from should be the previous HEAD");
        assert_ne!(updated.from.as_deref(), Some(updated.to.as_str()));
        assert!(
            updated.subjects.iter().any(|s| s.contains("update")),
            "subjects should contain the new commit, got {:?}",
            updated.subjects
        );

        let content = fs::read_to_string(dst.join("hello.txt")).unwrap();
        assert_eq!(content, "updated");
    }

    #[tokio::test]
    async fn test_sync_breaking_commit_detected() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_cmd(&src).args(["init"]).output().await.unwrap();
        fs::write(src.join("hello.txt"), "v1").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        // bang 形式の breaking commit を 1 件追加
        fs::write(src.join("hello.txt"), "v2").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "feat!: redesign"])
            .output()
            .await
            .unwrap();

        let change = repo.sync().await.unwrap().expect("HEAD moved");
        assert_eq!(change.breaking_subjects.len(), 1, "{:?}", change);
        assert!(change.breaking_subjects[0].contains("feat!: redesign"));
    }

    #[tokio::test]
    async fn test_update_returns_change_or_none() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_cmd(&src).args(["init"]).output().await.unwrap();
        fs::write(src.join("a.txt"), "a").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();

        // sync first to install
        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        // update with no remote changes → None
        assert!(repo.update().await.unwrap().is_none());

        // bump remote
        fs::write(src.join("a.txt"), "b").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "bump"])
            .output()
            .await
            .unwrap();

        let c = repo.update().await.unwrap().expect("HEAD moved");
        assert!(c.from.is_some());
        assert!(c.subjects.iter().any(|s| s.contains("bump")));
    }
}
