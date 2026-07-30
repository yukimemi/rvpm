use anyhow::{Context, Result};
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

    /// 既存 clone に対して **fetch せず** `rev` を checkout する。fetch cache の
    /// fast-path で「HEAD を effective_rev に揃えたいが window 内なので fetch は
    /// したくない」ケースに使う。rev が local DB に無ければエラーを返すので、
    /// caller は full sync にフォールバック (または `--no-refresh` なら error)
    /// する。`sync()` と同じく `Option<GitChange>` で HEAD 差分を返す。
    pub async fn checkout_locally(&self, rev: &str) -> Result<Option<GitChange>> {
        let dst = self.dst.to_path_buf();
        let rev = rev.to_string();
        tokio::task::spawn_blocking(move || checkout_local_impl(&dst, &rev))
            .await
            .map_err(|e| anyhow::anyhow!("checkout_locally task panicked: {}", e))?
    }

    /// `rev` (commit SHA / branch / tag) をローカルリポジトリで解決し、対応する
    /// commit SHA を返す。network を打たない。
    ///
    /// fetch cache の fast-path で「effective_rev (branch 名など) と local HEAD が
    /// 同じ commit を指してるか」を判定するために使う。commit SHA 同士の直接
    /// 比較だと `rev = "main"` / `rev = "v1.2.3"` 系をフォローできず、fast path
    /// の恩恵が失われるため。
    ///
    /// 未 clone / rev が local DB に無い / パースエラー → `Ok(None)` (caller は
    /// fast path 不適用として full flow に fall through する)。
    pub async fn resolve_revision_locally(&self, rev: &str) -> Result<Option<String>> {
        let dst = self.dst.to_path_buf();
        let rev = rev.to_string();
        tokio::task::spawn_blocking(move || resolve_revision_impl(&dst, &rev))
            .await
            .map_err(|e| anyhow::anyhow!("resolve_revision task panicked: {}", e))?
    }

    /// fetch 後の remote tracking branch の tip commit を返す。HEAD は動かさない。
    ///
    /// lockfile pin (rev なしで lockfile commit に寄せられているケース) が remote の
    /// 最新から乖離しているかを run_sync 側で判定するためのヘルパー。
    /// HEAD を読むわけではないので `head_commit()` と組み合わせて使う:
    /// `head != remote_head` なら「held back」。
    ///
    /// 解決順 (`gix_reset_to_remote` と同じロジック):
    /// 1. `refs/remotes/<remote>/<current_branch>`
    /// 2. `refs/remotes/<remote>/HEAD` (detached HEAD 時の fallback)
    ///
    /// どちらも解決できない場合は `None` (malformed repo、未 fetch 等)。
    /// caller は `None` を「判定不能」として扱い held-back 分類から除外する。
    pub async fn remote_head(&self) -> Result<Option<String>> {
        let dst = self.dst.to_path_buf();
        tokio::task::spawn_blocking(move || read_remote_head(&dst))
            .await
            .map_err(|e| anyhow::anyhow!("remote_head task panicked: {}", e))?
    }

    /// 既存 clone に対して fetch だけ実行する (HEAD は動かさない)。
    /// cooldown ゲート (#supply-chain) が「fetch → 判定 → checkout」を分割で
    /// 実行するためのプリミティブ。未 clone はエラー (fetch_impl 経由)。
    pub async fn fetch(&self) -> Result<()> {
        let dst = self.dst.to_path_buf();
        tokio::task::spawn_blocking(move || fetch_impl(&dst))
            .await
            .map_err(|e| anyhow::anyhow!("fetch task panicked: {}", e))?
    }

    /// `rev` の committer date を local DB から読む。未 clone / rev 解決不能 /
    /// 時刻が読めない場合は `Ok(None)` (caller は「コミット時刻不明 = cooldown の
    /// 熟成補助なし」として安全側に扱う)。network は打たない。
    pub async fn commit_time(&self, rev: &str) -> Result<Option<std::time::SystemTime>> {
        let dst = self.dst.to_path_buf();
        let rev = rev.to_string();
        tokio::task::spawn_blocking(move || commit_time_impl(&dst, &rev))
            .await
            .map_err(|e| anyhow::anyhow!("commit_time task panicked: {}", e))?
    }

    /// fetch 済みの remote tracking tip へ HEAD を進める (= `update()` から
    /// fetch を抜いたもの)。cooldown ゲートが fetch 済みの clone に Advance
    /// 判定を出した後の checkout に使う。`Option<GitChange>` の意味は
    /// `update()` と同じ (HEAD が動かなければ `None`)。
    pub async fn reset_to_remote_tip(&self) -> Result<Option<GitChange>> {
        let dst = self.dst.to_path_buf();
        tokio::task::spawn_blocking(move || reset_to_remote_tip_impl(&dst))
            .await
            .map_err(|e| anyhow::anyhow!("reset_to_remote_tip task panicked: {}", e))?
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

/// URL が remote (https://... / git@... / file://... 等) かどうか。
/// local path (`/`, `~`, `.`, `\`, `C:\` 始まり) は false。判定ルールは
/// `resolve_url` の local 判定と揃える。壊れた clone の自動削除が local path
/// (= user の dev dir の可能性) を巻き込まないためのガードに使う。
fn is_remote_url(url: &str) -> bool {
    url.contains("://") || url.contains('@')
}

// ======================================================
// clone / fetch — gix で in-process 実行
// checkout — gix の checkout API は複雑なため git コマンドにフォールバック
// status — gix で in-process 実行 (プロセス fork なし)
// ======================================================

fn sync_impl(url: &str, dst: &Path, rev: Option<&str>) -> Result<Option<GitChange>> {
    if dst.exists() {
        // `.git` が欠損した壊れた clone (disk 節約で dir だけ消した残骸、user
        // 報告の LuaSnip ケース) は、このままだと fetch_impl が
        // "does not appear to be a git repository" でコケるだけなので、ここで
        // 検知して fresh clone に fall through させる。
        // 削除は **remote URL の場合に限る** — local path URL の dst は user の
        // dev 作業 dir の可能性があり絶対に消せない。なお dev = true プラグ
        // インは run_sync / run_update が Repo::sync に到達する前に skip する
        // ので通常この経路には来ない (非 dev の local-path plugin = ミラー等
        // への保険ガード)。
        let broken = !dst.join(".git").exists() || gix::open(dst).is_err();
        if broken && is_remote_url(url) {
            eprintln!(
                "Warning: '{}' is not a valid git repository; removing it and re-cloning",
                dst.display()
            );
            std::fs::remove_dir_all(dst)?;
        }
    }
    if dst.exists() {
        let before = read_head(dst).ok();
        fetch_impl(dst)?;
        if let Some(rev) = rev {
            checkout_with_pin_fetch_retry(dst, rev)?;
        } else {
            gix_reset_to_remote(dst)?;
        }
        let after = read_head(dst)?;
        Ok(build_change(dst, before, after))
    } else {
        clone_impl(url, dst)?;
        if let Some(rev) = rev {
            // 新規 clone は default branch しか fetch されてない (`gix::prepare_clone`
            // の narrow refspec)。user が `rev = "v1"` 等の non-default branch を
            // 指定したケースは、ここで全 branch refspec で再 fetch して
            // `refs/remotes/origin/<rev>` を populate しないと checkout できない。
            // `fetch_impl` 自体が冒頭で `ensure_all_branches_refspec` を呼ぶので、
            // この経路で .git/config も同時に正しい状態になる。
            // `rev = "/regex/"` (タグ パターン) も同じ経路で OK — タグは shallow clone
            // でも `refs/tags/*` として一緒に降りてくるので、resolve_rev_for_checkout
            // が local DB から正しい候補を選べる。
            fetch_impl(dst)?;
            checkout_with_pin_fetch_retry(dst, rev)?;
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
        checkout_with_pin_fetch_retry(dst, rev)?;
    } else {
        gix_reset_to_remote(dst)?;
    }
    let after = read_head(dst)?;
    Ok(build_change(dst, before, after))
}

/// `rev` を local DB で解決して checkout する。
/// checkout 失敗時、`rev` (解決後) が **full 40 桁 lowercase hex SHA** (= rvpm.lock
/// の pin commit) の場合に限り、その commit を origin から depth 1 で個別 fetch
/// してから retry する。
///
/// 背景: clone は depth 1、fetch も Deepen(1) ずつなので、tip から 2 commit 以上
/// 離れた pin commit は local DB に存在しない。user が clone dir を削除してから
/// `rvpm sync` すると、再 clone 直後に rvpm.lock の古い commit へ checkout できず
/// "rev '<sha>' not found" が大量発生した (user 報告)。
///
/// branch 名 / tag / `/regex/` pattern が失敗した場合は従来どおり即エラー
/// (remote に無い branch を毎回 fetch しに行く等の無駄な往復を避ける)。
fn checkout_with_pin_fetch_retry(dst: &Path, rev: &str) -> Result<()> {
    let resolved = resolve_rev_for_checkout(dst, rev)?;
    match gix_checkout(dst, &resolved) {
        Ok(()) => Ok(()),
        Err(e) if is_full_hex_sha(&resolved) => {
            // pin commit だけを object id want で fetch してから retry。
            // fetch 自体が失敗した場合 (remote に commit が存在しない、server が
            // SHA want を許可していない等) は元の "rev not found" エラーに
            // fetch 失敗の情報を添えて返す。
            if let Err(fetch_err) = fetch_commit_by_sha(dst, &resolved) {
                return Err(e.context(format!(
                    "(also failed to fetch pinned commit from origin: {fetch_err:#})"
                )));
            }
            gix_checkout(dst, &resolved)
        }
        Err(e) => Err(e),
    }
}

/// `s` が full 40 桁 lowercase hex SHA (= lockfile pin) かどうか。
/// short SHA / 大文字混じりは対象外 (fetch-and-retry fallback は lockfile pin の
/// ケースだけに限定する)。
fn is_full_hex_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 特定 commit を SHA 指定で depth 1 fetch する (`git fetch --depth 1 origin <sha>` 相当)。
///
/// **gix の in-process fetch で実現している理由**: gix-refspec は one-sided fetch
/// refspec の src が full hex なら object id want として扱う
/// (`match_group::util::Needle::Object` — `git fetch origin <sha>` と同じ protocol
/// 上の振る舞い) ので、git CLI への shell out は不要。local ref は一切書かれず、
/// object DB に commit だけが降りる (detached HEAD への checkout は
/// `rev_parse_single(sha)` で直接解決できる)。
///
/// 注意: server 側が `uploadpack.allowAnySHA1InWant` (pin が ref から reachable なら
/// `allowReachableSHA1InWant`) を許可していないと失敗する。GitHub は reachable
/// commit の SHA fetch を許可している。test の local origin は git CLI の
/// upload-pack を使うため `uploadpack.allowAnySHA1InWant true` が必要。
fn fetch_commit_by_sha(dst: &Path, sha: &str) -> Result<()> {
    let repo = gix::open(dst)?;
    let remote = repo
        .find_default_remote(gix::remote::Direction::Fetch)
        .ok_or_else(|| anyhow::anyhow!("no remote configured"))??;
    let spec = gix::refspec::parse(sha.into(), gix::refspec::parse::Operation::Fetch)
        .map_err(|e| anyhow::anyhow!("failed to build refspec for pinned commit: {}", e))?
        .to_owned();
    let mut opts = gix::remote::ref_map::Options::default();
    opts.extra_refspecs.push(spec);
    remote
        .connect(gix::remote::Direction::Fetch)?
        .prepare_fetch(gix::progress::Discard, opts)?
        .with_shallow(gix::remote::fetch::Shallow::Deepen(1))
        .receive(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)?;
    Ok(())
}

/// clone path の HEAD commit hash を同期 (in-process gix) で読む。
/// view stamp の fingerprint 用 (#perf incremental generate)。 `.git` が無い
/// dev plugin や壊れた clone では Err — caller は None 化して「stamp 無効 =
/// 毎回 rebuild」へ安全側フォールバックする。
pub fn head_commit_of(dst: &Path) -> Result<String> {
    read_head(dst)
}

/// HEAD の commit hash を読み取る。failure は呼び出し側で None 化することもある。
fn read_head(dst: &Path) -> Result<String> {
    let repo = gix::open(dst)?;
    let head = repo.head_commit()?;
    Ok(head.id().to_string())
}

/// 既存 clone に対して fetch せず `rev` を checkout する。
/// rev が local DB に無い場合は `gix_checkout` がエラーを返す (caller で fallback)。
/// `rev = "/regex/"` (タグ パターン) は local DB に存在するタグだけから解決する
/// — 解決失敗 (パターンに合うタグが local に無い) もエラーで、caller は full sync
/// に fall through する (= sync_impl 経路で fetch 後に再解決される)。
fn checkout_local_impl(dst: &Path, rev: &str) -> Result<Option<GitChange>> {
    if !dst.exists() {
        anyhow::bail!("Plugin not installed: {}", dst.display());
    }
    let before = read_head(dst).ok();
    let resolved = resolve_rev_for_checkout(dst, rev)?;
    gix_checkout(dst, &resolved)?;
    let after = read_head(dst)?;
    Ok(build_change(dst, before, after))
}

/// `rev` を local DB で解決して **commit の** SHA 文字列を返す。
/// 未 clone / 未解決は `None`。
///
/// `rev_parse_single` 単独では annotated tag のときに tag object の SHA が返って
/// くる (commit SHA ではない)。そのまま local HEAD の commit SHA と比較すると
/// 常に不一致になり fast path が無効化されるので、git の `<rev>^{commit}` 記法で
/// tag chain を peel して commit に落とす。lightweight tag / branch / 生 SHA
/// ではこの記法は no-op なので副作用なし。
fn resolve_revision_impl(dst: &Path, rev: &str) -> Result<Option<String>> {
    if !dst.exists() {
        return Ok(None);
    }
    let repo = match gix::open(dst) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    // `rev = "/regex/"` (タグ パターン) は local タグから semver 最大を解決して
    // から rev_parse する。解決失敗 (= local DB に該当タグ無し) は caller の
    // fast-path 比較を「不一致」として落としたいだけなので `Ok(None)` で返す
    // (= caller は full sync に fall through する)。
    let resolved: std::borrow::Cow<str> = match parse_rev_pattern(rev) {
        Some(body) => match resolve_tag_pattern(&repo, body) {
            Ok(name) => std::borrow::Cow::Owned(name),
            Err(_) => return Ok(None),
        },
        None => std::borrow::Cow::Borrowed(rev),
    };
    let peeled = format!("{}^{{commit}}", resolved);
    if let Ok(id) = repo.rev_parse_single(&peeled[..]) {
        return Ok(Some(id.detach().to_string()));
    }
    // `^{commit}` が効かない edge case (gix が記法非対応の revision 形式等) の
    // 保険: plain parse を試す。
    match repo.rev_parse_single(resolved.as_ref()) {
        Ok(id) => Ok(Some(id.detach().to_string())),
        Err(_) => Ok(None),
    }
}

/// remote tracking branch の tip を読み取る。HEAD は動かさない。
/// tracking branch (`refs/remotes/<remote>/<branch>`) が見つからなければ
/// `refs/remotes/<remote>/HEAD` に fallback。それも無ければ `Ok(None)`。
fn read_remote_head(dst: &Path) -> Result<Option<String>> {
    let repo = gix::open(dst)?;
    let remote_name = repo
        .find_default_remote(gix::remote::Direction::Fetch)
        .and_then(|r| r.ok())
        .and_then(|r| r.name().map(|n| n.as_bstr().to_string()))
        .unwrap_or_else(|| "origin".to_string());

    // tracking ref が見つかっても peel 失敗時は `Ok(None)` に落とす (resilience:
    // malformed ref や stale packed-refs で held-back 判定全体が止まるのを避け、
    // 代わりにそのプラグインを「判定不能」として分類から除外する)。
    if let Some(head_name) = repo.head_name()? {
        let branch = head_name.as_bstr().to_string();
        let tracking = branch.replace("refs/heads/", &format!("refs/remotes/{}/", remote_name));
        if let Ok(mut tr) = repo.find_reference(&tracking)
            && let Ok(id) = tr.peel_to_id()
        {
            return Ok(Some(id.detach().to_string()));
        }
    }

    let remote_head_ref = format!("refs/remotes/{}/HEAD", remote_name);
    if let Ok(mut r) = repo.find_reference(&remote_head_ref)
        && let Ok(id) = r.peel_to_id()
    {
        return Ok(Some(id.detach().to_string()));
    }
    Ok(None)
}

/// `rev` の committer date を読む。失敗系はすべて `Ok(None)` に丸める
/// (resilience: cooldown 判定の補助情報が無いだけで処理は続行できる)。
fn commit_time_impl(dst: &Path, rev: &str) -> Result<Option<std::time::SystemTime>> {
    if !dst.exists() {
        return Ok(None);
    }
    let repo = match gix::open(dst) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    // annotated tag を commit まで peel する (`resolve_revision_impl` と同じ理由)。
    let peeled = format!("{}^{{commit}}", rev);
    let id = match repo
        .rev_parse_single(peeled.as_str())
        .or_else(|_| repo.rev_parse_single(rev))
    {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let commit = match repo.find_commit(id) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };
    let time = match commit.time() {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    if time.seconds < 0 {
        return Ok(None);
    }
    // `seconds as u64` is safe: guarded against negatives above. Use
    // `checked_add` rather than `+` so a corrupted / far-future committer
    // date can't panic — on Windows `SystemTime` is backed by `FILETIME`
    // (max year 30828) and the `+` operator panics on overflow. Overflow
    // degrades to `None`, matching this fn's "any problem → Ok(None)" contract.
    Ok(std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(time.seconds as u64)))
}

/// fetch なしで remote tracking tip へ HEAD を進める (update_impl の checkout 部)。
fn reset_to_remote_tip_impl(dst: &Path) -> Result<Option<GitChange>> {
    if !dst.exists() {
        anyhow::bail!("Plugin not installed: {}", dst.display());
    }
    let before = read_head(dst).ok();
    gix_reset_to_remote(dst)?;
    let after = read_head(dst)?;
    Ok(build_change(dst, before, after))
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
/// 失敗時 (repo open / rev parse / tree peel) は空 Vec (resilience)。
fn doc_files_changed(dst: &Path, from: &str, to: &str) -> Vec<String> {
    let Some((_repo, changes)) = open_and_diff(dst, from, to) else {
        return Vec::new();
    };
    let mut files: Vec<String> = changes
        .into_iter()
        .map(change_location)
        .filter(|p| is_doc_path(p))
        .collect();
    files.sort();
    files.dedup();
    files
}

/// repo open / rev parse / tree peel / tree diff をまとめて行うヘルパー。
/// 失敗時は `None` (resilience)。Rewrite tracking は明示的に無効化することで、
/// rename を Deletion + Addition の 2 件として返させる
/// (旧 `git diff --name-only` (rename detection 無し) と等価な挙動)。
fn open_and_diff(
    dst: &Path,
    from: &str,
    to: &str,
) -> Option<(
    gix::Repository,
    Vec<gix::object::tree::diff::ChangeDetached>,
)> {
    let repo = gix::open(dst).ok()?;
    // `from_tree` / `to_tree` borrow from `repo`; scope them in a block so they
    // drop before we move `repo` into the returned tuple.
    let changes = {
        let from_id = repo.rev_parse_single(from).ok()?;
        let to_id = repo.rev_parse_single(to).ok()?;
        let from_tree = repo.find_commit(from_id).ok()?.tree().ok()?;
        let to_tree = repo.find_commit(to_id).ok()?.tree().ok()?;
        let options = gix::diff::Options::default().with_rewrites(None);
        repo.diff_tree_to_tree(Some(&from_tree), Some(&to_tree), Some(options))
            .ok()?
    };
    Some((repo, changes))
}

/// `ChangeDetached` から destination path を返す。Rewrite tracking は
/// `open_and_diff` で無効化しているのでこの実装で出会わない想定だが、
/// 保険として location を返しておく (パスの重複は呼び出し側で `dedup`)。
fn change_location(change: gix::object::tree::diff::ChangeDetached) -> String {
    use gix::object::tree::diff::ChangeDetached;
    match change {
        ChangeDetached::Addition { location, .. }
        | ChangeDetached::Deletion { location, .. }
        | ChangeDetached::Modification { location, .. }
        | ChangeDetached::Rewrite { location, .. } => location.to_string(),
    }
}

/// path が "doc files" 集合 (top-level README*/CHANGELOG* + `doc/` 配下) に該当するか。
/// 旧実装の `git diff -- README* readme* Readme* CHANGELOG* changelog* Changelog* doc/`
/// と等価な集合を case-insensitive で表現する (top-level 限定なのは git pathspec の `*`
/// が `/` を跨がないため)。
fn is_doc_path(path: &str) -> bool {
    if let Some(rest) = path.strip_prefix("doc/") {
        return !rest.is_empty();
    }
    let top_level = !path.contains('/');
    if top_level {
        let lower = path.to_ascii_lowercase();
        return lower.starts_with("readme") || lower.starts_with("changelog");
    }
    false
}

/// `<from>..<to>` で `paths` に含まれるファイルそれぞれの unified diff をまとめて返す。
/// repo を 1 度だけ open して tree diff も 1 度だけ計算するので、`run_log --diff` が
/// 1 plugin × 多数 doc ファイルを処理するときの I/O を抑える。
/// repo open / rev parse / blob lookup 失敗時は当該 path の entry を結果に含めない
/// (resilience: 偽の空 diff を作らない)。
pub fn doc_file_patches(
    dst: &Path,
    from: &str,
    to: &str,
    paths: &[String],
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some((repo, changes)) = open_and_diff(dst, from, to) else {
        return out;
    };
    for path in paths {
        if let Some(patch) = build_patch_for_path(&repo, &changes, path) {
            out.insert(path.clone(), patch);
        }
    }
    out
}

/// 単一ファイルの patch 生成 (テスト用 thin wrapper)。本番経路は
/// `doc_file_patches` を使ってまとめて取得する。
#[cfg(test)]
fn doc_file_patch(dst: &Path, from: &str, to: &str, path: &str) -> Option<String> {
    doc_file_patches(dst, from, to, std::slice::from_ref(&path.to_string())).remove(path)
}

fn build_patch_for_path(
    repo: &gix::Repository,
    changes: &[gix::object::tree::diff::ChangeDetached],
    path: &str,
) -> Option<String> {
    use gix::object::tree::diff::ChangeDetached;

    let path_bytes = path.as_bytes();
    let change = changes.iter().find(|c| match c {
        ChangeDetached::Addition { location, .. }
        | ChangeDetached::Deletion { location, .. }
        | ChangeDetached::Modification { location, .. }
        | ChangeDetached::Rewrite { location, .. } => location.as_slice() == path_bytes,
    })?;

    let read_blob = |oid: gix::ObjectId| repo.find_blob(oid).ok().map(|b| b.detach().data);

    let (before, after, before_oid, after_oid) = match *change {
        ChangeDetached::Modification {
            previous_id, id, ..
        } => (
            read_blob(previous_id)?,
            read_blob(id)?,
            previous_id.to_string(),
            id.to_string(),
        ),
        ChangeDetached::Addition { id, .. } => (
            Vec::new(),
            read_blob(id)?,
            "0000000".to_string(),
            id.to_string(),
        ),
        ChangeDetached::Deletion { id, .. } => (
            read_blob(id)?,
            Vec::new(),
            id.to_string(),
            "0000000".to_string(),
        ),
        // Rewrite tracking は `open_and_diff` で無効化済み。万一 rename が
        // Rewrite で来たら destination → destination で素直に diff する
        // (source 側は Deletion として別 entry に分離されるはず)。
        ChangeDetached::Rewrite { source_id, id, .. } => (
            read_blob(source_id)?,
            read_blob(id)?,
            source_id.to_string(),
            id.to_string(),
        ),
    };

    Some(format_unified_diff(
        path,
        &before,
        &after,
        &before_oid,
        &after_oid,
    ))
}

/// git の null byte ヒューリスティック: 先頭 8KB に NUL があれば binary。
fn is_binary(buf: &[u8]) -> bool {
    let probe = &buf[..buf.len().min(8 * 1024)];
    probe.contains(&0u8)
}

fn format_unified_diff(
    path: &str,
    before: &[u8],
    after: &[u8],
    before_oid: &str,
    after_oid: &str,
) -> String {
    use gix::diff::blob::{
        Algorithm, Diff, InternedInput, UnifiedDiff,
        sources::byte_lines,
        unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader},
    };

    let short = |oid: &str| oid.get(..7).unwrap_or(oid).to_string();
    let mut out = String::new();
    out.push_str(&format!("diff --git a/{path} b/{path}\n"));
    out.push_str(&format!(
        "index {}..{}\n",
        short(before_oid),
        short(after_oid)
    ));

    if is_binary(before) || is_binary(after) {
        out.push_str(&format!("Binary files a/{path} and b/{path} differ\n"));
        return out;
    }

    out.push_str(&format!("--- a/{path}\n"));
    out.push_str(&format!("+++ b/{path}\n"));

    let input = InternedInput::new(byte_lines(before), byte_lines(after));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    struct Sink(String);
    impl ConsumeHunk for Sink {
        type Out = String;
        fn consume_hunk(
            &mut self,
            header: HunkHeader,
            lines: &[(DiffLineKind, &[u8])],
        ) -> std::io::Result<()> {
            // HunkHeader implements Display as `@@ -A,B +C,D @@`.
            self.0.push_str(&format!("{}\n", header));
            for (kind, line) in lines {
                self.0.push(kind.to_prefix());
                self.0.push_str(&String::from_utf8_lossy(line));
                if !line.ends_with(b"\n") {
                    self.0.push('\n');
                }
            }
            Ok(())
        }
        fn finish(self) -> Self::Out {
            self.0
        }
    }

    let body = UnifiedDiff::new(&diff, &input, Sink(String::new()), ContextSize::default())
        .consume()
        .unwrap_or_default();
    out.push_str(&body);
    out
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

    // clone 直後に refspec を全 branch に正規化しておくと、user が `rev = "v1"`
    // 等の非デフォルト branch を指定したケースで次回 fetch から拾える。
    // エラーを `?` で伝播 (Gemini #99 指摘): silent 握り潰しだと clone は成功した
    // のに後続 fetch で謎の "rev not found" になり原因究明が困難。
    ensure_all_branches_refspec(dst)?;

    Ok(())
}

fn fetch_impl(dst: &Path) -> Result<()> {
    // gix の prepare_clone は default で「default branch のみ」refspec を書く。
    // user が `rev = "v1"` のように非デフォルト branch を指定したとき rev_parse_single
    // が refs/remotes/origin/v1 を見つけられず "rev not found" になる。
    // → fetch のたびに `.git/config` の refspec を全 branch に正規化して
    //   次回以降 `git fetch` が全 branch を取れるようにする (idempotent)。
    ensure_all_branches_refspec(dst)?;

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

/// `.git/config` の `[remote "origin"] fetch = ...` を全 branch refspec に正規化する。
///
/// gix の `prepare_clone` は default で `refs/heads/<default>:refs/remotes/origin/<default>`
/// だけを書くが、これだと user が `rev = "v1"` (= origin の v1 branch) を指定したとき、
/// fetch しても v1 が remote tracking ref として作られず checkout できない。
///
/// git CLI の標準動作 (`+refs/heads/*:refs/remotes/origin/*`) に揃えれば、以降の
/// fetch_impl で全 branch が `refs/remotes/origin/<branch>` として取れる。
///
/// 既存 .git/config でも同じ問題があるので、fetch のたびにこの関数を呼ぶ
/// (idempotent: 既に正しい設定なら no-op)。
fn ensure_all_branches_refspec(dst: &Path) -> Result<()> {
    let config_path = dst.join(".git").join("config");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return Ok(()), // .git/config が無いなら fetch 側でエラーになるので静観
    };
    let want = "+refs/heads/*:refs/remotes/origin/*";
    if content.contains(want) {
        return Ok(());
    }
    // `[remote "origin"]` セクション内の `fetch = ...` 行を全 branch refspec に置換。
    // セクション境界は次の `[...]` 行か EOF。
    //
    // 旧実装は append 経路で「`replaced = false` なら末尾に追記」していたが、
    // `[remote "origin"]` の後に他のセクションが続いていると新 fetch 行が誤って
    // 末尾セクション (例: `[branch "main"]`) の所属になっていた (Gemini High 指摘)。
    // → 今は **iterate 中に origin セクションのスコープを追跡し、フェッチ行が
    //   無いまま origin が閉じる瞬間に注入する**。EOF までに見つからなければ
    //   末尾に origin セクションごと追加する。
    let mut new_content = String::with_capacity(content.len() + 64);
    let mut in_origin_section = false;
    let mut replaced = false;
    let mut pending_origin_fetch_inject = false;
    let leading_ws_default = "\t"; // git config の慣習
    for line in content.lines() {
        let trimmed = line.trim_start();
        let starts_section = trimmed.starts_with('[');

        // 既に origin セクション内で fetch 行未発見、かつ次のセクション開始 →
        // ここで fetch 行を origin の所属として注入してから次セクションへ進む。
        if starts_section && pending_origin_fetch_inject {
            new_content.push_str(leading_ws_default);
            new_content.push_str("fetch = ");
            new_content.push_str(want);
            new_content.push('\n');
            pending_origin_fetch_inject = false;
            replaced = true;
        }

        if starts_section {
            // 新しいセクション開始
            in_origin_section = trimmed.starts_with("[remote \"origin\"]")
                || trimmed.starts_with("[remote 'origin']");
            if in_origin_section {
                // origin に入った瞬間に「fetch 行を注入したい」状態に入れる。
                // この後の行で `fetch = ...` が見つかれば置換に切り替えて
                // pending を解除する。
                pending_origin_fetch_inject = true;
            }
        } else if in_origin_section
            && let Some(idx) = trimmed.find("fetch")
            && trimmed[idx..]
                .trim_start_matches("fetch")
                .trim_start()
                .starts_with('=')
        {
            // `fetch = ...` 行を上書き
            let leading_ws = &line[..line.len() - line.trim_start().len()];
            new_content.push_str(leading_ws);
            new_content.push_str("fetch = ");
            new_content.push_str(want);
            new_content.push('\n');
            replaced = true;
            pending_origin_fetch_inject = false;
            continue;
        }
        new_content.push_str(line);
        new_content.push('\n');
    }
    // EOF までに origin セクション内で fetch 行を一度も見ていない場合 (= origin が
    // 最後のセクションで `fetch = ...` 自体が無いケース)。pending_origin_fetch_inject
    // が立っていれば末尾に挿入。
    if pending_origin_fetch_inject {
        new_content.push_str(leading_ws_default);
        new_content.push_str("fetch = ");
        new_content.push_str(want);
        new_content.push('\n');
        replaced = true;
    }
    // origin セクションそのものが無いケース (rvpm が clone した直後なら必ずあるが、
    // .git/config が手動で壊された等のガード)。末尾に新規セクションを足す。
    if !replaced && !new_content.contains("[remote \"origin\"]") {
        new_content.push_str("[remote \"origin\"]\n");
        new_content.push_str(leading_ws_default);
        new_content.push_str("fetch = ");
        new_content.push_str(want);
        new_content.push('\n');
    }
    std::fs::write(&config_path, new_content)?;
    Ok(())
}

/// gix で特定の rev に checkout。branch の場合は branch を維持。
///
/// rev 解決順 (git CLI の `git checkout <rev>` と挙動を揃える):
///   1. `rev_parse_single(rev)` — 直接 ref / tag / SHA を試す
///   2. (1) が失敗で rev が non-default branch のとき: `refs/remotes/origin/<rev>` を
///      明示的に試して、ローカル branch を作る (git CLI の auto-track 相当)
///
/// 旧実装は (1) のみだったので `rev = "v1"` 等の非デフォルト branch は、`.git/config`
/// が全 branch refspec を持ち remote tracking ref も存在していても "rev not found"
/// になっていた (#user 報告)。
fn gix_checkout(dst: &Path, rev: &str) -> Result<()> {
    let repo = gix::open(dst)?;

    // (1) 直接解決
    let direct = repo.rev_parse_single(rev);
    let (commit_id, source) = match direct {
        Ok(id) => (id.detach(), DirectOrRemote::Direct),
        Err(_) => {
            // (2) refs/remotes/origin/<rev> を試す (= remote tracking branch)
            let remote_ref = format!("refs/remotes/origin/{rev}");
            let remote_id = repo
                .find_reference(&remote_ref)
                .ok()
                .and_then(|mut r| r.peel_to_id().ok())
                .ok_or_else(|| anyhow::anyhow!("rev '{}' not found", rev))?;
            (remote_id.detach(), DirectOrRemote::FromRemote)
        }
    };

    // rev が local branch (refs/heads/<rev>) を指す or 上記 (2) で remote から
    // 拾ったケースのどちらでも、symbolic HEAD で local branch を立てる。
    // (2) のとき local branch がまだ無ければ作る (= git CLI の `checkout <branch>`
    // で自動 tracking branch を作るのと同じ振る舞い)。
    let branch_ref = format!("refs/heads/{}", rev);
    let local_branch_exists = repo.find_reference(&branch_ref).is_ok();
    // local branch が既にあれば必ず symbolic HEAD で track。それが無くても
    // remote から拾った場合は新規作成する (`git checkout` の auto-tracking 相当)。
    // `Direct && exists` のチェックは `local_branch_exists` に内包されるので冗長 (Gemini 指摘)。
    let should_set_branch = local_branch_exists || matches!(source, DirectOrRemote::FromRemote);

    if should_set_branch {
        let head_path = repo.git_dir().join("HEAD");
        std::fs::write(&head_path, format!("ref: {}\n", branch_ref))?;
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

/// `gix_checkout` の rev 解決経路 (debug / test 用)。
#[derive(Debug, Clone, Copy)]
enum DirectOrRemote {
    /// `rev_parse_single` で直接解決できた (local branch / tag / SHA)。
    Direct,
    /// `refs/remotes/origin/<rev>` から拾った (= remote tracking branch fallback)。
    FromRemote,
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
        // `/regex/` 形式は local タグから semver 最大を解決してから存在確認。
        // 解決失敗 = local DB に対象タグが無い → Error として表面化させる
        // (`rvpm doctor` / status 経路で気付けるように)。
        let target: std::borrow::Cow<str> = match parse_rev_pattern(rev) {
            Some(body) => match resolve_tag_pattern(&repo, body) {
                Ok(name) => std::borrow::Cow::Owned(name),
                Err(e) => {
                    return RepoStatus::Error(format!(
                        "rev pattern '{}' unresolved in local repo: {}",
                        rev, e
                    ));
                }
            },
            None => std::borrow::Cow::Borrowed(rev),
        };
        match repo.rev_parse_single(target.as_ref()) {
            Ok(_) => {}
            Err(_) => {
                return RepoStatus::Error(format!("rev '{}' not found in local repo", target));
            }
        }
    }

    RepoStatus::Clean
}

// ======================================================
// rev pattern resolution (`rev = "/regex/"` → semver-max tag)
// ======================================================

/// `rev` 文字列が `/regex/` 形式かを判定し、内部の regex 本体を返す。
/// それ以外 (literal タグ / branch / SHA) は `None`。
///
/// `on_cmd` / `on_event` / `on_map` の `/regex/` 区切りと同じ構文 (#85, #88) で、
/// rvpm 全体での一貫性を保つ。空 body (`"//"`) は判定対象外 (None)。
pub(crate) fn parse_rev_pattern(rev: &str) -> Option<&str> {
    rev.strip_prefix('/')
        .and_then(|s| s.strip_suffix('/'))
        .filter(|s| !s.is_empty())
}

/// タグ名から先頭の `v` / `V` プレフィックスを 1 個だけ剥がす。
/// `v1.0.0` / `V2.3.1` → `1.0.0` / `2.3.1`。プレフィックスが無ければそのまま。
fn strip_v_prefix(tag: &str) -> &str {
    tag.strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag)
}

/// 候補タグの iterator から regex マッチ + semver パース可能なものだけを取り、
/// 最大 semver の tag 名を返す。
///
/// パース失敗タグは候補から外す (resilience: `release-pre` のような非 semver
/// タグが混じっていても黙って無視する。lazy.nvim と同じ挙動)。候補ゼロなら
/// `Ok(None)` を返し、呼び出し側がエラー文言を組み立てる。
///
/// 入力は **owning iterator**: 本番経路は gix の `references().tags()` から直接
/// 流し込み、 ピーク使用量を O(1) に抑える (Gemini PR #134 指摘の最適化 — 中間
/// `Vec<String>` を作らない)。 テストは `vec!["v1.0.0".into(), ...]` を渡して
/// pure helper として呼べる。
fn pick_max_semver_tag<I>(tags: I, regex_body: &str) -> Result<Option<String>>
where
    I: IntoIterator<Item = String>,
{
    let re = regex::Regex::new(regex_body)
        .with_context(|| format!("invalid regex in rev pattern: '/{}/'", regex_body))?;
    let mut best: Option<(semver::Version, String)> = None;
    for tag in tags {
        if !re.is_match(&tag) {
            continue;
        }
        let parsed = match semver::Version::parse(strip_v_prefix(&tag)) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let take = best.as_ref().is_none_or(|(cur, _)| parsed > *cur);
        if take {
            best = Some((parsed, tag));
        }
    }
    Ok(best.map(|(_, name)| name))
}

/// `repo` の local DB にあるタグから、regex にマッチして semver パース可能な
/// 最大バージョンを選び、タグ名 (e.g. `"v1.6.4"`) を返す。 候補ゼロは error。
///
/// gix の references iterator を `pick_max_semver_tag` に直接食わせ、 全タグ名を
/// 同時に保持する中間 `Vec<String>` を作らない (PR #134 Gemini 指摘)。
fn resolve_tag_pattern(repo: &gix::Repository, regex_body: &str) -> Result<String> {
    let platform = repo.references()?;
    // refs/tags/<name> から `<name>` を抽出。 壊れた ref は無視 (resilience)。
    let names = platform.tags()?.filter_map(|r| r.ok()).filter_map(|r| {
        let full = r.name().as_bstr().to_string();
        full.strip_prefix("refs/tags/").map(str::to_string)
    });
    pick_max_semver_tag(names, regex_body)?.ok_or_else(|| {
        anyhow::anyhow!(
            "rev pattern '/{}/' matched no parseable semver tag",
            regex_body,
        )
    })
}

/// `rev` がパターンなら local タグから解決、リテラルならそのまま返す。
/// sync_impl / update_impl / checkout_local_impl で gix_checkout の手前に挟む。
fn resolve_rev_for_checkout(dst: &Path, rev: &str) -> Result<String> {
    match parse_rev_pattern(rev) {
        Some(body) => {
            let repo = gix::open(dst)?;
            resolve_tag_pattern(&repo, body)
        }
        None => Ok(rev.to_string()),
    }
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

    // Ensures the test process itself has a committer identity gix can find,
    // for code paths (Repo::sync / Repo::update / Repo::checkout_locally) that
    // create or mutate dst repos via gix-in-process — those don't go through
    // `git_cmd`'s per-Command env, so they need either repo-local config or
    // process-level env vars. We use env vars because dst repos are created
    // *by* gix (sync) and we don't have a hook to write their .git/config.
    //
    // `Once` keeps this safe under the parallel test runner: env mutation
    // happens exactly once, before any test reads the env.
    fn ensure_committer_env() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            // SAFETY: called on first git_init_with_user, before any test
            // gix-call. No other test code mutates these vars, so racing
            // reads from gix are stable after this single write.
            unsafe {
                std::env::set_var("GIT_AUTHOR_NAME", "test");
                std::env::set_var("GIT_AUTHOR_EMAIL", "test@test.com");
                std::env::set_var("GIT_COMMITTER_NAME", "test");
                std::env::set_var("GIT_COMMITTER_EMAIL", "test@test.com");
            }
        });
    }

    // `git init` + write `[user]` into the repo's local `.git/config` so that
    // gix-based code paths (Repo::checkout_locally, sync_impl, …) running
    // inside the test process can find a committer for reflog updates. The
    // env vars set on `git_cmd` only reach the spawned `git` CLI; they do
    // NOT propagate to the parent test process where gix actually executes —
    // hence both the per-repo write and the process-level env var below.
    async fn git_init_with_user(dir: &Path) {
        ensure_committer_env();
        git_cmd(dir).args(["init"]).output().await.unwrap();
        git_cmd(dir)
            .args(["config", "user.name", "test"])
            .output()
            .await
            .unwrap();
        git_cmd(dir)
            .args(["config", "user.email", "test@test.com"])
            .output()
            .await
            .unwrap();
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
        git_init_with_user(&src).await;
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
        git_init_with_user(&src).await;
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
        git_init_with_user(&src).await;
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
        git_init_with_user(&src).await;
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
        git_init_with_user(&src).await;
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
        git_init_with_user(&src).await;
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

    async fn git_head(dir: &Path) -> String {
        let out = git_cmd(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[tokio::test]
    async fn test_remote_head_reports_tracking_branch_tip() {
        // Mirrors the "held back by lockfile pin" scenario: pin to an old
        // commit, advance the remote, verify that remote_head reflects the
        // new remote tip while HEAD stays at the pin.
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "v1").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let initial = git_head(&src).await;

        // Fresh clone → local HEAD == remote tip.
        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();
        assert_eq!(
            repo.remote_head().await.unwrap().as_deref(),
            Some(initial.as_str()),
            "fresh clone: remote_head should match HEAD"
        );

        // Advance the remote by one commit.
        fs::write(src.join("a.txt"), "v2").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "advance"])
            .output()
            .await
            .unwrap();
        let new_tip = git_head(&src).await;
        assert_ne!(new_tip, initial, "remote tip must have moved");

        // Re-sync with the pinned rev: fetch brings the new ref in, but
        // HEAD stays at `initial`.
        let pinned = Repo::new(src.to_str().unwrap(), &dst, Some(initial.as_str()));
        pinned.sync().await.unwrap();
        assert_eq!(
            pinned.head_commit().await.unwrap(),
            initial,
            "pinned sync must keep HEAD at the requested rev"
        );

        // remote_head must return the NEW tip, signalling the held-back state.
        let rh = pinned.remote_head().await.unwrap();
        assert_eq!(
            rh.as_deref(),
            Some(new_tip.as_str()),
            "remote_head must report the fetched remote tip, not HEAD"
        );
        assert_ne!(rh.as_deref(), Some(initial.as_str()));
    }

    #[tokio::test]
    async fn test_fetch_only_updates_tracking_ref_without_moving_head() {
        // cooldown gate primitive: `fetch()` must bring the new remote tip
        // into the local DB (visible via remote_head) while HEAD stays put.
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "v1").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let initial = git_head(&src).await;

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        fs::write(src.join("a.txt"), "v2").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "advance"])
            .output()
            .await
            .unwrap();
        let new_tip = git_head(&src).await;

        repo.fetch().await.unwrap();

        assert_eq!(
            repo.head_commit().await.unwrap(),
            initial,
            "fetch must not move HEAD"
        );
        assert_eq!(
            repo.remote_head().await.unwrap().as_deref(),
            Some(new_tip.as_str()),
            "fetch must update the remote tracking tip"
        );
        // Worktree untouched too.
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "v1");
    }

    #[tokio::test]
    async fn test_reset_to_remote_tip_advances_head_without_network() {
        // After a separate fetch, reset_to_remote_tip must move HEAD (and the
        // worktree) to the tracking tip and report the GitChange — the
        // "Advance" half of the cooldown gate.
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "v1").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let initial = git_head(&src).await;

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        fs::write(src.join("a.txt"), "v2").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "advance"])
            .output()
            .await
            .unwrap();
        let new_tip = git_head(&src).await;

        repo.fetch().await.unwrap();
        let change = repo
            .reset_to_remote_tip()
            .await
            .unwrap()
            .expect("HEAD moved");
        assert_eq!(change.from.as_deref(), Some(initial.as_str()));
        assert_eq!(change.to, new_tip);
        assert_eq!(repo.head_commit().await.unwrap(), new_tip);
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "v2");

        // No-op second call → None.
        assert!(repo.reset_to_remote_tip().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_commit_time_reads_committer_date() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "v1").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .env("GIT_COMMITTER_DATE", "2020-01-02T03:04:05Z")
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();
        let head = repo.head_commit().await.unwrap();

        // 2020-01-02T03:04:05Z = 1577934245 unix
        let expected = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_934_245);
        assert_eq!(repo.commit_time(&head).await.unwrap(), Some(expected));

        // Unresolvable rev / missing clone degrade to None, not Err.
        assert_eq!(repo.commit_time("no-such-rev").await.unwrap(), None);
        let never_cloned = root.path().join("nope");
        let missing = Repo::new("dummy", &never_cloned, None);
        assert_eq!(missing.commit_time("HEAD").await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_resolve_revision_locally_handles_sha_branch_tag_and_missing() {
        // Fast-path comparison depends on being able to resolve branch/tag
        // refs to SHAs locally without hitting the network. Exercise all
        // four cases (full SHA / branch / tag / bogus) from a single repo.
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "seed").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        git_cmd(&src)
            .args(["tag", "v1.0.0"])
            .output()
            .await
            .unwrap();
        let head_sha = git_head(&src).await;
        let branch = {
            let out = git_cmd(&src)
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .output()
                .await
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        // Full SHA round-trips.
        assert_eq!(
            repo.resolve_revision_locally(&head_sha).await.unwrap(),
            Some(head_sha.clone()),
        );
        // Branch name resolves to the same SHA.
        assert_eq!(
            repo.resolve_revision_locally(&branch).await.unwrap(),
            Some(head_sha.clone()),
        );
        // Tag name resolves to the same SHA.
        assert_eq!(
            repo.resolve_revision_locally("v1.0.0").await.unwrap(),
            Some(head_sha.clone()),
        );
        // Nonexistent rev degrades to None (caller falls through to full sync).
        assert_eq!(
            repo.resolve_revision_locally("no-such-rev").await.unwrap(),
            None,
        );
    }

    #[tokio::test]
    async fn test_resolve_revision_locally_returns_none_on_missing_clone() {
        let root = tempdir().unwrap();
        let dst = root.path().join("never-cloned");
        let repo = Repo::new("dummy", &dst, None);
        assert_eq!(repo.resolve_revision_locally("HEAD").await.unwrap(), None,);
    }

    #[tokio::test]
    async fn test_resolve_revision_locally_peels_annotated_tag_to_commit() {
        // Annotated tags are backed by their own tag object whose SHA differs
        // from the commit they point at. Plain `rev_parse_single` returns the
        // tag-object SHA, which would never match HEAD and silently disable
        // the fast path. Verify we peel to the underlying commit.
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "seed").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        git_cmd(&src)
            .args(["tag", "-a", "v2.0.0", "-m", "annotated"])
            .output()
            .await
            .unwrap();
        let head_sha = git_head(&src).await;

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        assert_eq!(
            repo.resolve_revision_locally("v2.0.0").await.unwrap(),
            Some(head_sha),
            "annotated tag must resolve to the target commit SHA",
        );
    }

    #[tokio::test]
    async fn test_checkout_locally_moves_head_to_existing_commit() {
        // --no-refresh path: HEAD at commit B, user wants A, and A is already
        // in the local object DB. `checkout_locally` must move HEAD without
        // talking to the network. We build the DB directly with `git init` +
        // two commits in dst, bypassing `repo.sync()` — sync uses a shallow
        // (depth-1) clone that would not keep the older commit locally and
        // would mask the exact code path we want to exercise.
        let root = tempdir().unwrap();
        let dst = root.path().join("dst");

        fs::create_dir_all(&dst).unwrap();
        git_init_with_user(&dst).await;
        fs::write(dst.join("a.txt"), "v1").unwrap();
        git_cmd(&dst).args(["add", "."]).output().await.unwrap();
        git_cmd(&dst)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let first = git_head(&dst).await;
        fs::write(dst.join("a.txt"), "v2").unwrap();
        git_cmd(&dst).args(["add", "."]).output().await.unwrap();
        git_cmd(&dst)
            .args(["commit", "-m", "bump"])
            .output()
            .await
            .unwrap();
        let second = git_head(&dst).await;

        let repo = Repo::new("dummy", &dst, None);
        assert_eq!(repo.head_commit().await.unwrap(), second);

        let change = repo.checkout_locally(&first).await.unwrap();
        assert!(
            change.is_some(),
            "HEAD should have moved, expected a GitChange"
        );
        assert_eq!(repo.head_commit().await.unwrap(), first);

        // Re-checkout of the same rev is a no-op (None GitChange).
        let change = repo.checkout_locally(&first).await.unwrap();
        assert!(change.is_none(), "re-checkout of same rev should be no-op");
    }

    #[test]
    fn ensure_all_branches_refspec_replaces_narrow_default_refspec() {
        // gix の prepare_clone は default で `.../<default>:.../<default>` の narrow
        // な refspec を書く。これだと `rev = "v1"` 等の非デフォルト branch が
        // fetch されない (issue: user 報告で rev 'v1' not found)。
        // この helper で `+refs/heads/*:refs/remotes/origin/*` (= git CLI の default)
        // に正規化される。
        let tmp = tempdir().unwrap();
        let dst = tmp.path();
        fs::create_dir_all(dst.join(".git")).unwrap();
        let initial = "[remote \"origin\"]\n\turl = https://github.com/foo/bar\n\tfetch = +refs/heads/main:refs/remotes/origin/main\n";
        fs::write(dst.join(".git/config"), initial).unwrap();

        ensure_all_branches_refspec(dst).unwrap();

        let after = fs::read_to_string(dst.join(".git/config")).unwrap();
        assert!(
            after.contains("+refs/heads/*:refs/remotes/origin/*"),
            "should rewrite to all-branch refspec: {after}"
        );
        assert!(
            !after.contains("refs/remotes/origin/main"),
            "narrow refspec should be replaced, not duplicated: {after}"
        );
    }

    #[test]
    fn ensure_all_branches_refspec_is_idempotent_when_already_correct() {
        let tmp = tempdir().unwrap();
        let dst = tmp.path();
        fs::create_dir_all(dst.join(".git")).unwrap();
        let already_correct =
            "[remote \"origin\"]\n\turl = x\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n";
        fs::write(dst.join(".git/config"), already_correct).unwrap();

        ensure_all_branches_refspec(dst).unwrap();

        let after = fs::read_to_string(dst.join(".git/config")).unwrap();
        // 1 行だけ存在することを確認 (重複追記してない)
        assert_eq!(
            after.matches("fetch = ").count(),
            1,
            "refspec should not be duplicated: {after}"
        );
    }

    #[test]
    fn ensure_all_branches_refspec_only_touches_origin_section() {
        // 他の remote セクションの fetch 行は触らない (rvpm は origin だけ管理)。
        let tmp = tempdir().unwrap();
        let dst = tmp.path();
        fs::create_dir_all(dst.join(".git")).unwrap();
        let mixed = "[remote \"upstream\"]\n\tfetch = +refs/heads/main:refs/remotes/upstream/main\n[remote \"origin\"]\n\tfetch = +refs/heads/main:refs/remotes/origin/main\n";
        fs::write(dst.join(".git/config"), mixed).unwrap();

        ensure_all_branches_refspec(dst).unwrap();

        let after = fs::read_to_string(dst.join(".git/config")).unwrap();
        assert!(
            after.contains("upstream/main"),
            "upstream section must be preserved: {after}"
        );
        assert!(
            after.contains("+refs/heads/*:refs/remotes/origin/*"),
            "origin should be normalized: {after}"
        );
    }

    #[test]
    fn ensure_all_branches_refspec_inserts_into_origin_when_origin_is_not_last_section() {
        // 旧実装は `replaced = false` 経路で末尾に append していたが、`[remote "origin"]`
        // が中間にある config だと新 fetch 行が **後続セクション** (例: `[branch "main"]`)
        // の所属になっていた (Gemini High 指摘 #99)。
        // 修正後: origin スコープを iterate 中に追跡し、次セクション開始 or EOF 直前に
        // 注入する。
        let tmp = tempdir().unwrap();
        let dst = tmp.path();
        fs::create_dir_all(dst.join(".git")).unwrap();
        // origin セクションには fetch 行が **無い**、後続に branch セクション。
        let initial = "[remote \"origin\"]\n\turl = https://github.com/foo/bar\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n";
        fs::write(dst.join(".git/config"), initial).unwrap();

        ensure_all_branches_refspec(dst).unwrap();

        let after = fs::read_to_string(dst.join(".git/config")).unwrap();
        // fetch 行は origin セクション内 (= branch セクションの **前**) にあるべき
        let fetch_pos = after
            .find("fetch = +refs/heads/*")
            .expect("fetch line written");
        let branch_pos = after
            .find("[branch \"main\"]")
            .expect("branch section preserved");
        assert!(
            fetch_pos < branch_pos,
            "fetch line must be inside [remote \"origin\"], i.e. BEFORE [branch \"main\"]:\n{after}"
        );
        // branch セクションの内容が壊れていないこと
        assert!(after.contains("merge = refs/heads/main"));
    }

    #[tokio::test]
    async fn test_sync_resolves_non_default_branch_via_full_refspec() {
        // 非デフォルト branch 名 (e.g. `v1`) を rev に指定したとき、fetch が
        // ちゃんと remote tracking ref を作って checkout が成功することを確認。
        // user 報告: blink.cmp の `rev = "v1"` が "rev not found" になるバグの
        // 回帰 test。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        // **最重要**: `git init` 直後の default branch を確定させる (CodeRabbit
        // PR #99 review 指摘)。後で v1 を作って checkout するので、この段階で
        // default を控えておかないと、setup 末尾で「現在の HEAD = v1」を
        // default だと誤認識して checkout を skip し、clone 元 src の HEAD が
        // v1 のまま残ってしまう。すると `gix::prepare_clone` が v1 を default
        // として cloning し、`rev_parse_single("v1")` の direct path だけで
        // 解決してしまうので、この test の本来の対象 (refs/remotes/origin/v1
        // fallback path) が exercise されなくなる。
        let init_head = git_cmd(&src)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .await
            .expect("symbolic-ref HEAD just after init");
        let default_branch = String::from_utf8_lossy(&init_head.stdout)
            .trim()
            .to_string();
        assert_ne!(
            default_branch, "v1",
            "test invariant: init default must not be v1"
        );

        // master/main 上に commit
        fs::write(src.join("a.txt"), "main-1").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "main"])
            .output()
            .await
            .unwrap();
        // v1 branch を作って別 commit
        git_cmd(&src)
            .args(["checkout", "-b", "v1"])
            .output()
            .await
            .unwrap();
        fs::write(src.join("a.txt"), "v1-1").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "v1"])
            .output()
            .await
            .unwrap();
        let v1_head = git_head(&src).await;

        // src を default branch に戻す。これで `gix::prepare_clone` 時の
        // default ref は v1 ではなく default_branch になり、
        // `gix_checkout(dst, "v1")` は `refs/remotes/origin/v1` の fallback path
        // を経由して解決される (= この test の主旨)。
        git_cmd(&src)
            .args(["checkout", &default_branch])
            .output()
            .await
            .expect("checkout init default before clone");

        let url = format!("file://{}", src.display());
        let repo = Repo::new(&url, &dst, Some("v1"));
        repo.sync()
            .await
            .expect("sync to v1 should succeed after refspec normalization");

        // v1 の HEAD に揃っていること
        let head = repo.head_commit().await.unwrap();
        assert_eq!(head, v1_head, "checkout should land on v1 tip");
    }

    #[tokio::test]
    async fn test_checkout_locally_errors_when_rev_not_present() {
        // The commit isn't in the local object DB → error. Caller uses this
        // signal to fall through to full sync (or surface under --no-refresh).
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "only").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        let result = repo
            .checkout_locally("ffffffffffffffffffffffffffffffffffffffff")
            .await;
        assert!(
            result.is_err(),
            "unknown rev must error, not silently succeed"
        );
    }

    #[tokio::test]
    async fn test_update_returns_change_or_none() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
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

    #[test]
    fn test_is_doc_path() {
        assert!(is_doc_path("README"));
        assert!(is_doc_path("README.md"));
        assert!(is_doc_path("readme.txt"));
        assert!(is_doc_path("ReadMe"));
        assert!(is_doc_path("CHANGELOG"));
        assert!(is_doc_path("CHANGELOG.md"));
        assert!(is_doc_path("changelog"));
        assert!(is_doc_path("doc/foo.txt"));
        assert!(is_doc_path("doc/sub/bar.txt"));
        assert!(!is_doc_path(""));
        assert!(!is_doc_path("doc/"));
        assert!(!is_doc_path("docs/foo.txt")); // not "doc/"
        assert!(!is_doc_path("src/README.md")); // not top-level
        assert!(!is_doc_path("Cargo.toml"));
    }

    /// `<from>..<to>` で README.md / doc/ の変更が拾え、無関係ファイルが落ちる。
    #[tokio::test]
    async fn test_doc_files_changed_filters_to_doc_set() {
        let root = tempdir().unwrap();
        let src = root.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(src.join("doc")).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("README.md"), "v1\n").unwrap();
        fs::write(src.join("doc/intro.txt"), "hello\n").unwrap();
        fs::write(src.join("src.txt"), "code v1\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let from = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        fs::write(src.join("README.md"), "v2\n").unwrap();
        fs::write(src.join("doc/intro.txt"), "world\n").unwrap();
        fs::write(src.join("src.txt"), "code v2\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "bump"])
            .output()
            .await
            .unwrap();
        let to = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let mut files = doc_files_changed(&src, &from, &to);
        files.sort();
        assert_eq!(
            files,
            vec!["README.md".to_string(), "doc/intro.txt".to_string()]
        );
    }

    /// repo open / rev parse 失敗で空 Vec (resilience)。
    #[tokio::test]
    async fn test_doc_files_changed_resilient_to_missing_repo() {
        let root = tempdir().unwrap();
        let nowhere = root.path().join("nowhere");
        let files = doc_files_changed(&nowhere, "deadbeef", "cafebabe");
        assert!(files.is_empty());
    }

    /// unified diff の hunk header と +/- 行が想定どおり生成される。
    #[tokio::test]
    async fn test_doc_file_patch_emits_unified_diff() {
        let root = tempdir().unwrap();
        let src = root.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("README.md"), "alpha\nbeta\ngamma\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let from = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        fs::write(src.join("README.md"), "alpha\nBETA\ngamma\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "bump"])
            .output()
            .await
            .unwrap();
        let to = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let patch = doc_file_patch(&src, &from, &to, "README.md").expect("patch");
        assert!(patch.contains("diff --git a/README.md b/README.md"));
        assert!(patch.contains("--- a/README.md"));
        assert!(patch.contains("+++ b/README.md"));
        assert!(patch.contains("@@"));
        assert!(patch.contains("-beta"));
        assert!(patch.contains("+BETA"));
    }

    /// 追加ファイルでも patch が出る (`/dev/null` 起点でなくても header は出す)。
    #[tokio::test]
    async fn test_doc_file_patch_handles_addition() {
        let root = tempdir().unwrap();
        let src = root.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("README.md"), "v1\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let from = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        fs::write(src.join("CHANGELOG.md"), "first release\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "add cl"])
            .output()
            .await
            .unwrap();
        let to = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let patch = doc_file_patch(&src, &from, &to, "CHANGELOG.md").expect("patch");
        assert!(patch.contains("diff --git a/CHANGELOG.md b/CHANGELOG.md"));
        assert!(patch.contains("+first release"));
    }

    /// バイナリ blob は `Binary files ... differ` の 1 行に丸める。
    #[tokio::test]
    async fn test_doc_file_patch_handles_binary_blob() {
        let root = tempdir().unwrap();
        let src = root.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        // null-byte を含む binary 風 blob (現実的には PNG / dat 等の `doc/` 内画像)。
        fs::create_dir_all(src.join("doc")).unwrap();
        fs::write(
            src.join("doc/asset.bin"),
            [0xFFu8, 0x00, 0xAB, 0x00, b'd', b'\n'],
        )
        .unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let from = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        fs::write(src.join("doc/asset.bin"), [0x00u8, 0x01, 0x02, 0x03, b'\n']).unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "bump"])
            .output()
            .await
            .unwrap();
        let to = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let patch = doc_file_patch(&src, &from, &to, "doc/asset.bin").expect("patch");
        assert!(patch.contains("diff --git a/doc/asset.bin b/doc/asset.bin"));
        assert!(patch.contains("Binary files a/doc/asset.bin and b/doc/asset.bin differ"));
        // bin だと unified hunk は出ない (early return)。
        assert!(!patch.contains("@@"));
    }

    /// blob 取得が失敗してもパス自体が tree diff に含まれていない (= 無関係) なら
    /// `None`。`unwrap_or_default` を使っていないことを担保する回帰テスト。
    #[tokio::test]
    async fn test_doc_file_patches_skips_paths_not_in_diff() {
        let root = tempdir().unwrap();
        let src = root.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("README.md"), "v1\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let from = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        fs::write(src.join("README.md"), "v2\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "bump"])
            .output()
            .await
            .unwrap();
        let to = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let paths = vec!["README.md".to_string(), "ghost.md".to_string()];
        let patches = doc_file_patches(&src, &from, &to, &paths);
        assert!(patches.contains_key("README.md"));
        assert!(!patches.contains_key("ghost.md"));
    }

    /// `gix_diff::blob::sources::byte_lines` は token に改行を含むので、
    /// unified diff の output で行と行が連結しない。retro-fix 防止。
    #[tokio::test]
    async fn test_doc_file_patch_lines_are_separated() {
        let root = tempdir().unwrap();
        let src = root.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("README.md"), "alpha\nbeta\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let from = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        fs::write(src.join("README.md"), "ALPHA\nBETA\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "bump"])
            .output()
            .await
            .unwrap();
        let to = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let patch = doc_file_patch(&src, &from, &to, "README.md").expect("patch");
        // 行が `-alpha-beta` のように連結していたら byte_lines が改行を切り捨てている。
        assert!(patch.contains("-alpha\n"));
        assert!(patch.contains("-beta\n"));
        assert!(patch.contains("+ALPHA\n"));
        assert!(patch.contains("+BETA\n"));
    }

    /// 該当ファイルが diff に含まれない場合は `None`。
    #[tokio::test]
    async fn test_doc_file_patch_returns_none_for_unchanged_path() {
        let root = tempdir().unwrap();
        let src = root.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("README.md"), "v1\n").unwrap();
        fs::write(src.join("other.txt"), "stable\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let from = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        fs::write(src.join("README.md"), "v2\n").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "bump"])
            .output()
            .await
            .unwrap();
        let to = String::from_utf8(
            git_cmd(&src)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        assert!(doc_file_patch(&src, &from, &to, "other.txt").is_none());
    }

    // ======================================================
    // rev pattern (`rev = "/regex/"` → semver-max tag)
    // ======================================================

    #[test]
    fn test_parse_rev_pattern_detects_slash_delimited() {
        // `/regex/` = pattern。それ以外 (literal タグ / branch / SHA / 半端な
        // `/foo` 形式) は None。空 body (`//`) も None (filter で除外)。
        assert_eq!(parse_rev_pattern("/v1\\..*/"), Some("v1\\..*"));
        assert_eq!(parse_rev_pattern("/^v1$/"), Some("^v1$"));
        // literal はそのまま透過 (None)。
        assert_eq!(parse_rev_pattern("v1.0.0"), None);
        assert_eq!(parse_rev_pattern("main"), None);
        assert_eq!(parse_rev_pattern("abcd1234"), None);
        // 片側だけ slash や空 body は pattern として扱わない。
        assert_eq!(parse_rev_pattern("/v1"), None);
        assert_eq!(parse_rev_pattern("v1/"), None);
        assert_eq!(parse_rev_pattern("//"), None);
    }

    #[test]
    fn test_strip_v_prefix_handles_v_and_no_prefix() {
        assert_eq!(strip_v_prefix("v1.0.0"), "1.0.0");
        assert_eq!(strip_v_prefix("V2.3.1"), "2.3.1");
        assert_eq!(strip_v_prefix("1.0.0"), "1.0.0");
        // 2 文字目の `v` は剥がさない (1 個だけ)。
        assert_eq!(strip_v_prefix("vv1.0.0"), "v1.0.0");
    }

    #[test]
    fn test_pick_max_semver_tag_picks_highest_match() {
        // `/v1\..*/` で v1.x のみ抽出 → 1.10.0 が最大 (lex sort なら 1.2.0 が
        // 勝つので、ここで semver 順が効いてることを確認)。
        let tags = vec![
            "v1.0.0".to_string(),
            "v1.2.0".to_string(),
            "v1.10.0".to_string(),
            "v2.0.0".to_string(),
            "v0.9.5".to_string(),
        ];
        let pick = pick_max_semver_tag(tags.clone(), r"^v1\.").unwrap();
        assert_eq!(pick, Some("v1.10.0".to_string()));
    }

    #[test]
    fn test_pick_max_semver_tag_ignores_unparseable_tags() {
        // semver パース不可のタグ (release-pre 等) はマッチしても候補に含めない。
        // パターン全マッチでも候補ゼロなら None。
        let tags = vec![
            "release-1.0".to_string(),
            "rc-2".to_string(),
            "v1.0.0".to_string(),
        ];
        // `/^v/` で v1.0.0 だけ semver 通る → それを選ぶ。
        assert_eq!(
            pick_max_semver_tag(tags.clone(), r"^v").unwrap(),
            Some("v1.0.0".to_string())
        );
        // `/^release/` は release-* がマッチするけど semver 不通 → None。
        assert_eq!(
            pick_max_semver_tag(tags.clone(), r"^release").unwrap(),
            None
        );
    }

    #[test]
    fn test_pick_max_semver_tag_handles_prerelease_ordering() {
        // semver の prerelease は通常リリースより小さい (1.0.0-rc1 < 1.0.0)。
        let tags = vec![
            "v1.0.0-rc.1".to_string(),
            "v1.0.0".to_string(),
            "v1.0.0-rc.2".to_string(),
        ];
        assert_eq!(
            pick_max_semver_tag(tags.clone(), r"^v1\.").unwrap(),
            Some("v1.0.0".to_string())
        );
    }

    #[test]
    fn test_pick_max_semver_tag_invalid_regex_errors() {
        let tags = vec!["v1.0.0".to_string()];
        let err = pick_max_semver_tag(tags.clone(), r"[unbalanced").unwrap_err();
        assert!(err.to_string().contains("invalid regex"));
    }

    #[tokio::test]
    async fn test_resolve_tag_pattern_against_real_repo() {
        // タグ列挙経路 (gix references API) も含めて end-to-end で確認。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "seed").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        for tag in ["v1.0.0", "v1.2.0", "v1.10.0", "v2.0.0", "wip"] {
            git_cmd(&src).args(["tag", tag]).output().await.unwrap();
        }

        let repo = gix::open(&src).unwrap();
        let pick = resolve_tag_pattern(&repo, r"^v1\.").unwrap();
        assert_eq!(pick, "v1.10.0");
    }

    #[tokio::test]
    async fn test_resolve_tag_pattern_errors_when_no_match() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "seed").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        git_cmd(&src)
            .args(["tag", "v0.1.0"])
            .output()
            .await
            .unwrap();

        let repo = gix::open(&src).unwrap();
        let err = resolve_tag_pattern(&repo, r"^v9\.").unwrap_err();
        assert!(err.to_string().contains("matched no parseable semver tag"));
    }

    #[tokio::test]
    async fn test_sync_with_rev_pattern_lands_on_max_tag() {
        // sync_impl 経路全体で pattern → tag 解決 → checkout が動くこと。
        // user の典型: `rev = "/^v1\\..*/"` で blink.cmp 系の "must be on a tag"
        // 警告を回避。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "seed").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        // v1.0.0 を打つ。
        git_cmd(&src)
            .args(["tag", "v1.0.0"])
            .output()
            .await
            .unwrap();
        // commit を進めて v1.10.0 を打つ。lex sort なら v1.2 が勝つ並びに
        // しておく (= semver 順を効かせるテスト)。
        fs::write(src.join("a.txt"), "v1.10").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "bump"])
            .output()
            .await
            .unwrap();
        git_cmd(&src)
            .args(["tag", "v1.10.0"])
            .output()
            .await
            .unwrap();
        let v1_10_sha = git_head(&src).await;
        // さらに v2.0.0 を進めて打つ → /^v1\\..*/ で v1.10.0 が選ばれることを確認。
        fs::write(src.join("a.txt"), "v2").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "v2"])
            .output()
            .await
            .unwrap();
        git_cmd(&src)
            .args(["tag", "v2.0.0"])
            .output()
            .await
            .unwrap();
        // v2 commit に default branch が乗ったままだと shallow clone で v1 commit
        // が降りないので、default branch を v1 commit に戻しておく
        // (タグ自体は降りるけど、`refs/tags/v1.10.0 → commit` の `commit` を
        //  local DB に持ってる必要があるので。実 GitHub 相当 (タグ commit が
        //  reachable) を再現)。
        git_cmd(&src)
            .args(["reset", "--hard", "v1.10.0"])
            .output()
            .await
            .unwrap();

        let url = format!("file://{}", src.display());
        let repo = Repo::new(&url, &dst, Some("/^v1\\..*/"));
        repo.sync().await.expect("sync with rev pattern");

        let head = repo.head_commit().await.unwrap();
        assert_eq!(head, v1_10_sha, "should land on v1.10.0 commit");
    }

    #[tokio::test]
    async fn test_sync_with_rev_pattern_errors_when_no_tag_matches() {
        // パターンに合うタグが remote に無いケース → fetch 後の解決でエラー。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "seed").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        git_cmd(&src)
            .args(["tag", "v0.1.0"])
            .output()
            .await
            .unwrap();

        let url = format!("file://{}", src.display());
        let repo = Repo::new(&url, &dst, Some("/^v9\\..*/"));
        let err = repo.sync().await.unwrap_err();
        assert!(
            err.to_string().contains("matched no parseable semver tag"),
            "actual: {}",
            err
        );
    }

    // ─── shallow clone × lockfile pin 救済 (#shallow-pin-fallback) ──────────
    // depth-1 clone + Deepen(1) fetch では tip から 2 commit 以上離れた pin
    // commit が local DB に無い。checkout 失敗時に pin commit を object id
    // want で個別 fetch して retry する fallback の回帰テスト群。

    /// SHA fetch を許可する設定を origin repo に入れる。
    /// gix の file transport は `git upload-pack` を spawn するが、git CLI の
    /// upload-pack はデフォルトで非 advertise の SHA want を拒否する。
    /// (GitHub は reachable commit の SHA fetch を許可しているので本番では
    /// 問題にならないが、local origin のテストでは明示が必要。)
    async fn allow_any_sha1_in_want(dir: &Path) {
        git_cmd(dir)
            .args(["config", "uploadpack.allowAnySHA1InWant", "true"])
            .output()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_sync_fetches_pinned_sha_missing_from_fresh_clone() {
        // user 報告の再現: clone dir を削除 → rvpm sync が depth-1 で再 clone
        // → rvpm.lock の古い pin commit が local DB に無く "rev not found"。
        // fresh clone 経路でも fallback が発動して pin commit に checkout
        // できることを確認。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        allow_any_sha1_in_want(&src).await;
        let pinned = commit_to(&src, "v1", "init").await;
        commit_to(&src, "v2", "bump-2").await;
        let tip = commit_to(&src, "v3", "bump-3").await;

        // depth-1 clone (= tip のみ) + fetch_impl の Deepen(1) (= 1 commit
        // 深掘り) では pinned (2 commit 前) は降りてこない。
        let repo = Repo::new(src.to_str().unwrap(), &dst, Some(pinned.as_str()));
        repo.sync()
            .await
            .expect("sync must recover the pinned commit via per-SHA fetch");
        assert_eq!(repo.head_commit().await.unwrap(), pinned);
        assert_ne!(repo.head_commit().await.unwrap(), tip);
        assert_eq!(fs::read_to_string(dst.join("hello.txt")).unwrap(), "v1");
    }

    #[tokio::test]
    async fn test_sync_existing_clone_fetches_missing_pinned_sha() {
        // 既存 clone 経路でも同じ fallback が効くこと。rev=None で sync した
        // depth-1 clone (= tip のみ保持) に対し、2 commit 前の pin を指定して
        // 再 sync すると、Deepen(1) だけでは pin commit は降りてこない。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        allow_any_sha1_in_want(&src).await;
        let pinned = commit_to(&src, "v1", "init").await;
        commit_to(&src, "v2", "bump-2").await;
        commit_to(&src, "v3", "bump-3").await;

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        let pinned_repo = Repo::new(src.to_str().unwrap(), &dst, Some(pinned.as_str()));
        pinned_repo
            .sync()
            .await
            .expect("existing clone must also recover the pinned commit");
        assert_eq!(pinned_repo.head_commit().await.unwrap(), pinned);
        assert_eq!(fs::read_to_string(dst.join("hello.txt")).unwrap(), "v1");
    }

    #[tokio::test]
    async fn test_update_fetches_missing_pinned_sha() {
        // update_impl 経路でも同じ fallback が効くこと。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        allow_any_sha1_in_want(&src).await;
        let pinned = commit_to(&src, "v1", "init").await;
        commit_to(&src, "v2", "bump-2").await;
        commit_to(&src, "v3", "bump-3").await;

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        let pinned_repo = Repo::new(src.to_str().unwrap(), &dst, Some(pinned.as_str()));
        pinned_repo
            .update()
            .await
            .expect("update must recover the pinned commit via per-SHA fetch");
        assert_eq!(pinned_repo.head_commit().await.unwrap(), pinned);
    }

    #[tokio::test]
    async fn test_sync_missing_branch_rev_keeps_original_error() {
        // 40-hex SHA 以外 (branch 名等) の checkout 失敗は fetch-and-retry を
        // 発動せず、従来どおりの "rev not found" エラーを返す。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        commit_to(&src, "v1", "init").await;

        let repo = Repo::new(src.to_str().unwrap(), &dst, Some("no-such-branch"));
        let err = repo.sync().await.unwrap_err();
        assert!(
            err.to_string().contains("rev 'no-such-branch' not found"),
            "actual: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_sync_unfetchable_pinned_sha_surfaces_error() {
        // remote に存在しない 40-hex SHA は fallback の fetch 自体も失敗する
        // ので、最終的にエラーが表面化する (silent success しない)。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        allow_any_sha1_in_want(&src).await;
        commit_to(&src, "v1", "init").await;

        let repo = Repo::new(
            src.to_str().unwrap(),
            &dst,
            Some("ffffffffffffffffffffffffffffffffffffffff"),
        );
        assert!(repo.sync().await.is_err());
    }

    #[tokio::test]
    async fn test_sync_reclones_when_dst_is_not_a_git_repo() {
        // user 報告の LuaSnip ケース: dir だけ残って `.git` が無い壊れた clone
        // は、fetch_impl が "not a git repository" でコケる代わりに、削除して
        // fresh clone に fall through する。remote URL (file:// 含む) 限定 —
        // local path URL は user の dev dir の可能性があるので絶対に消さない。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        commit_to(&src, "hello", "init").await;

        let url = format!("file://{}", src.display());
        let repo = Repo::new(&url, &dst, None);
        repo.sync().await.unwrap();
        assert!(dst.join(".git").exists());

        // `.git` を消して壊れた状態を再現。
        fs::remove_dir_all(dst.join(".git")).unwrap();
        repo.sync()
            .await
            .expect("broken clone must be removed and re-cloned");
        assert!(dst.join(".git").exists());
        assert_eq!(fs::read_to_string(dst.join("hello.txt")).unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_sync_keeps_local_path_dst_even_if_broken() {
        // local path URL (= dev / mirror の可能性) の dst は壊れていても絶対に
        // 削除しない — 従来どおりのエラーで止まる (データ損壊防止ガード)。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        commit_to(&src, "hello", "init").await;

        // dst を「.git の無い壊れた dir」として作る。
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("hello.txt"), "precious user data").unwrap();

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        assert!(
            repo.sync().await.is_err(),
            "local-path dst must not be auto-removed"
        );
        // 中身が消えていないこと。
        assert_eq!(
            fs::read_to_string(dst.join("hello.txt")).unwrap(),
            "precious user data"
        );
    }

    #[tokio::test]
    async fn test_resolve_revision_locally_with_pattern() {
        // fast-path 比較経路: pattern → tag 解決 → commit SHA が返ること。
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("a.txt"), "seed").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        git_cmd(&src)
            .args(["tag", "v1.0.0"])
            .output()
            .await
            .unwrap();
        let head_sha = git_head(&src).await;

        let repo = Repo::new(src.to_str().unwrap(), &dst, None);
        repo.sync().await.unwrap();

        // pattern も literal タグも同じ commit SHA を返す。
        assert_eq!(
            repo.resolve_revision_locally("/^v1\\.")
                .await
                .ok()
                .flatten(),
            None,
            "片側 slash は pattern 扱いせず literal として rev_parse → None"
        );
        assert_eq!(
            repo.resolve_revision_locally("/^v1\\..*/").await.unwrap(),
            Some(head_sha.clone()),
            "pattern が解決→commit SHA",
        );
    }

    #[tokio::test]
    async fn test_update_single_plugin_syncs_missing() {
        let root = tempdir().unwrap();
        let src = root.path().join("src");
        let dst = root.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("hello.txt"), "hello").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();

        let plugin = crate::config::Plugin {
            url: src.to_str().unwrap().to_string(),
            dst: Some(dst.to_str().unwrap().to_string()),
            ..Default::default()
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        let cache_root = root.path().join("cache");

        let res = crate::update_single_plugin(&plugin, &cache_root, tx, None).await;

        // TDD: This will fail on the current codebase because the plugin is missing and not synced yet.
        assert!(
            res.is_ok(),
            "Expected update_single_plugin to succeed by syncing the missing plugin, but got: {:?}",
            res
        );

        assert!(dst.join("hello.txt").exists());
        assert_eq!(fs::read_to_string(dst.join("hello.txt")).unwrap(), "hello");

        let mut statuses = Vec::new();
        while let Ok((_, status)) = rx.try_recv() {
            statuses.push(status);
        }
        assert_eq!(statuses.len(), 2);
        assert!(matches!(statuses[0], crate::PluginStatus::Syncing(ref m) if m == "Syncing..."));
        assert!(matches!(statuses[1], crate::PluginStatus::Finished));
    }

    // ─── cooldown-gated update (#supply-chain) ─────────────────────────────
    // End-to-end behavior of `update_single_plugin` with a `PluginCooldownCtx`:
    // fresh tips are held, matured tips advance, old-by-commit-date tips
    // advance, and a matured intermediate observation becomes the fallback.

    use crate::cooldown::{ObservedCommit, PluginCooldownCtx};
    use crate::update_log::format_rfc3339_utc;

    const DAY_SECS: u64 = 24 * 60 * 60;

    /// src repo (1 commit) + cloned dst + Plugin を作る共通セットアップ。
    /// (plugin, src, dst, cache_root, initial_head) を返す。
    async fn setup_cloned_plugin(
        root: &Path,
    ) -> (
        crate::config::Plugin,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        String,
    ) {
        let src = root.join("src");
        let dst = root.join("dst");
        fs::create_dir_all(&src).unwrap();
        git_init_with_user(&src).await;
        fs::write(src.join("hello.txt"), "v1").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .args(["commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        let initial = git_head(&src).await;

        let plugin = crate::config::Plugin {
            url: src.to_str().unwrap().to_string(),
            dst: Some(dst.to_str().unwrap().to_string()),
            ..Default::default()
        };
        let cache_root = root.join("cache");
        let (tx, _rx) = tokio::sync::mpsc::channel(100);
        crate::update_single_plugin(&plugin, &cache_root, tx, None)
            .await
            .unwrap();
        (plugin, src, dst, cache_root, initial)
    }

    async fn commit_to(src: &Path, content: &str, msg: &str) -> String {
        fs::write(src.join("hello.txt"), content).unwrap();
        git_cmd(src).args(["add", "."]).output().await.unwrap();
        git_cmd(src)
            .args(["commit", "-m", msg])
            .output()
            .await
            .unwrap();
        git_head(src).await
    }

    #[tokio::test]
    async fn test_update_single_plugin_cooldown_holds_fresh_tip() {
        let root = tempdir().unwrap();
        let (plugin, src, dst, cache_root, initial) = setup_cloned_plugin(root.path()).await;
        let new_tip = commit_to(&src, "v2", "advance").await;

        // First-ever observation of the fresh tip → must hold at the old HEAD.
        let ctx = PluginCooldownCtx {
            cooldown: std::time::Duration::from_secs(DAY_SECS),
            observed: Vec::new(),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(100);
        let (_p, change, head, outcome) =
            crate::update_single_plugin(&plugin, &cache_root, tx, Some(ctx))
                .await
                .unwrap();

        assert!(change.is_none(), "held update must not move HEAD");
        assert_eq!(head.as_deref(), Some(initial.as_str()));
        assert_eq!(fs::read_to_string(dst.join("hello.txt")).unwrap(), "v1");
        let out = outcome.expect("cooldown outcome must be reported");
        let held = out.held.expect("fresh tip must be flagged as held");
        assert_eq!(held.tip, new_tip);
        assert_eq!(held.fallback, None);
        assert!(
            out.observed.iter().any(|o| o.commit == new_tip),
            "tip must be recorded as observed so it can mature"
        );
    }

    #[tokio::test]
    async fn test_update_single_plugin_cooldown_advances_matured_tip() {
        let root = tempdir().unwrap();
        let (plugin, src, dst, cache_root, initial) = setup_cloned_plugin(root.path()).await;
        let new_tip = commit_to(&src, "v2", "advance").await;

        // Pretend we first saw this tip 2 days ago (cooldown = 1 day).
        let two_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * DAY_SECS);
        let ctx = PluginCooldownCtx {
            cooldown: std::time::Duration::from_secs(DAY_SECS),
            observed: vec![ObservedCommit {
                commit: new_tip.clone(),
                first_seen: format_rfc3339_utc(two_days_ago),
                committed_at: None,
            }],
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(100);
        let (_p, change, head, outcome) =
            crate::update_single_plugin(&plugin, &cache_root, tx, Some(ctx))
                .await
                .unwrap();

        let change = change.expect("matured tip must be applied");
        assert_eq!(change.from.as_deref(), Some(initial.as_str()));
        assert_eq!(change.to, new_tip);
        assert_eq!(head.as_deref(), Some(new_tip.as_str()));
        assert_eq!(fs::read_to_string(dst.join("hello.txt")).unwrap(), "v2");
        assert!(outcome.unwrap().held.is_none());
    }

    #[tokio::test]
    async fn test_update_single_plugin_cooldown_advances_old_commit_date() {
        // Dormant repo: the new tip's committer date is ancient → applies
        // immediately even on first observation.
        let root = tempdir().unwrap();
        let (plugin, src, dst, cache_root, _initial) = setup_cloned_plugin(root.path()).await;
        fs::write(src.join("hello.txt"), "v2").unwrap();
        git_cmd(&src).args(["add", "."]).output().await.unwrap();
        git_cmd(&src)
            .env("GIT_COMMITTER_DATE", "2020-01-02T03:04:05Z")
            .args(["commit", "-m", "old advance"])
            .output()
            .await
            .unwrap();
        let new_tip = git_head(&src).await;

        let ctx = PluginCooldownCtx {
            cooldown: std::time::Duration::from_secs(DAY_SECS),
            observed: Vec::new(),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(100);
        let (_p, change, head, outcome) =
            crate::update_single_plugin(&plugin, &cache_root, tx, Some(ctx))
                .await
                .unwrap();

        assert_eq!(change.expect("old commit must be applied").to, new_tip);
        assert_eq!(head.as_deref(), Some(new_tip.as_str()));
        assert_eq!(fs::read_to_string(dst.join("hello.txt")).unwrap(), "v2");
        assert!(outcome.unwrap().held.is_none());
    }

    #[tokio::test]
    async fn test_update_single_plugin_cooldown_falls_back_to_matured_observed() {
        // Active repo: tip is too fresh, but an intermediate commit observed
        // long enough ago must be checked out instead (delayed following).
        let root = tempdir().unwrap();
        let (plugin, src, dst, cache_root, initial) = setup_cloned_plugin(root.path()).await;

        // mid becomes tip, gets fetched (= how it entered the local DB in
        // real usage when it was observed), then tip lands on top.
        let mid = commit_to(&src, "v2", "mid").await;
        {
            let repo = Repo::new(&plugin.url, &dst, None);
            repo.fetch().await.unwrap();
        }
        let new_tip = commit_to(&src, "v3", "tip").await;

        let now = std::time::SystemTime::now();
        let ctx = PluginCooldownCtx {
            cooldown: std::time::Duration::from_secs(DAY_SECS),
            observed: vec![
                ObservedCommit {
                    commit: initial.clone(),
                    first_seen: format_rfc3339_utc(
                        now - std::time::Duration::from_secs(10 * DAY_SECS),
                    ),
                    committed_at: None,
                },
                ObservedCommit {
                    commit: mid.clone(),
                    first_seen: format_rfc3339_utc(
                        now - std::time::Duration::from_secs(2 * DAY_SECS),
                    ),
                    committed_at: None,
                },
            ],
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(100);
        let (_p, change, head, outcome) =
            crate::update_single_plugin(&plugin, &cache_root, tx, Some(ctx))
                .await
                .unwrap();

        let change = change.expect("fallback checkout must move HEAD");
        assert_eq!(change.from.as_deref(), Some(initial.as_str()));
        assert_eq!(change.to, mid);
        assert_eq!(head.as_deref(), Some(mid.as_str()));
        assert_eq!(fs::read_to_string(dst.join("hello.txt")).unwrap(), "v2");
        let held = outcome.unwrap().held.expect("tip itself is still held");
        assert_eq!(held.tip, new_tip);
        assert_eq!(held.fallback.as_deref(), Some(mid.as_str()));
    }
}
