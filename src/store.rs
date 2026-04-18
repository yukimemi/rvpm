use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// GitHub Search API のレスポンス。
#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    #[allow(dead_code)]
    pub total_count: u64,
    pub items: Vec<GitHubRepo>,
}

/// GitHub リポジトリ情報。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubRepo {
    pub full_name: String,
    pub html_url: String,
    pub description: Option<String>,
    pub stargazers_count: u64,
    pub updated_at: String,
    pub topics: Vec<String>,
    pub default_branch: Option<String>,
}

impl GitHubRepo {
    /// プラグイン名 (repo 部分)。
    pub fn plugin_name(&self) -> &str {
        self.full_name
            .split('/')
            .next_back()
            .unwrap_or(&self.full_name)
    }

    /// stars を人間可読な形式に。
    pub fn stars_display(&self) -> String {
        if self.stargazers_count >= 1000 {
            format!("{:.1}k", self.stargazers_count as f64 / 1000.0)
        } else {
            self.stargazers_count.to_string()
        }
    }

    /// README の raw URL。
    pub fn readme_url(&self) -> String {
        let branch = self.default_branch.as_deref().unwrap_or("main");
        format!(
            "https://raw.githubusercontent.com/{}/{}/README.md",
            self.full_name, branch
        )
    }
}

/// キャッシュディレクトリ。`<cache_root>/store/` に配置。
/// 呼び出し元から cache_root を渡すことで `options.cache_root` を尊重する。
fn store_cache_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("store")
}

/// 検索結果のキャッシュパス。
fn search_cache_path(cache_root: &Path, query: &str) -> PathBuf {
    let safe_name: String = query
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    store_cache_dir(cache_root).join(format!("search_{}.json", safe_name))
}

/// README のキャッシュパス。
fn readme_cache_path(cache_root: &Path, full_name: &str) -> PathBuf {
    let safe_name = full_name.replace('/', "__");
    store_cache_dir(cache_root)
        .join("readme")
        .join(format!("{}.md", safe_name))
}

/// キャッシュファイルが有効期間内か。
fn is_cache_valid(path: &Path, max_age: std::time::Duration) -> bool {
    path.metadata()
        .and_then(|m| m.modified())
        .map(|t| {
            t.elapsed()
                .unwrap_or(max_age + std::time::Duration::from_secs(1))
                < max_age
        })
        .unwrap_or(false)
}

const SEARCH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(86400); // 24h
const README_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(604800); // 7 days
/// 取得する最大ページ数 (100 件/ページ)。unauth rate limit (60 req/h) を食い潰さない
/// 範囲に抑える。3 ページ = 300 件で体感上十分。
const MAX_PAGES: u32 = 3;

/// GitHub Search API でプラグインを検索。キャッシュがあればそれを返す。
/// 複数ページ取得 (最大 `MAX_PAGES` * 100 件) し、合算結果をキャッシュする。
pub fn search_plugins(cache_root: &Path, query: &str) -> Result<Vec<GitHubRepo>> {
    // キャッシュチェック
    let cache_path = search_cache_path(cache_root, query);
    if is_cache_valid(&cache_path, SEARCH_CACHE_TTL)
        && let Ok(data) = std::fs::read_to_string(&cache_path)
        && let Ok(repos) = serde_json::from_str::<Vec<GitHubRepo>>(&data)
    {
        return Ok(repos);
    }

    // GitHub API 検索 — reqwest の query() で安全にエンコード
    let search_query = if query.is_empty() {
        "topic:neovim-plugin".to_string()
    } else {
        format!("topic:neovim-plugin {}", query)
    };

    let client = reqwest::blocking::Client::builder()
        .user_agent("rvpm")
        .build()?;

    let mut all_items: Vec<GitHubRepo> = Vec::new();
    // ページ境界で失敗した場合、取得済みの部分結果は呼び出し元に返すが、
    // 不完全なデータは 24h キャッシュに書かない (次回 TUI 復帰時に再試行させる)。
    let mut cache_complete = true;
    for page in 1..=MAX_PAGES {
        let page_str = page.to_string();
        let resp = match client
            .get("https://api.github.com/search/repositories")
            .query(&[
                ("q", search_query.as_str()),
                ("sort", "stars"),
                ("order", "desc"),
                ("per_page", "100"),
                ("page", page_str.as_str()),
            ])
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                if all_items.is_empty() {
                    return Err(e.into());
                }
                // TUI が active かもしれないので eprintln! はしない。
                cache_complete = false;
                break;
            }
        };

        let parsed: SearchResponse = match resp.json() {
            Ok(p) => p,
            Err(e) => {
                if all_items.is_empty() {
                    return Err(e.into());
                }
                cache_complete = false;
                break;
            }
        };

        if parsed.items.is_empty() {
            // 正常終了 (ページ尽き)
            break;
        }
        let got_full_page = parsed.items.len() >= 100;
        all_items.extend(parsed.items);
        if !got_full_page {
            break;
        }
    }

    // 完全取得できたときだけキャッシュに保存
    if cache_complete && !all_items.is_empty() {
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        if let Ok(json) = serde_json::to_string(&all_items) {
            std::fs::write(&cache_path, json).ok();
        }
    }

    Ok(all_items)
}

/// 人気プラグインのランキングを取得。
pub fn fetch_popular(cache_root: &Path) -> Result<Vec<GitHubRepo>> {
    search_plugins(cache_root, "")
}

/// README を取得。キャッシュがあればそれを返す。
pub fn fetch_readme(cache_root: &Path, repo: &GitHubRepo) -> Result<String> {
    let cache_path = readme_cache_path(cache_root, &repo.full_name);
    if is_cache_valid(&cache_path, README_CACHE_TTL)
        && let Ok(data) = std::fs::read_to_string(&cache_path)
    {
        return Ok(data);
    }

    let url = repo.readme_url();
    let client = reqwest::blocking::Client::builder()
        .user_agent("rvpm")
        .build()?;

    let resp = client.get(&url).send()?;
    let text = if resp.status().is_success() {
        resp.text()?
    } else {
        // main で見つからない場合は master を試す
        let url_master = url.replace("/main/README.md", "/master/README.md");
        let resp2 = client.get(&url_master).send()?;
        if resp2.status().is_success() {
            resp2.text()?
        } else {
            "README not found.".to_string()
        }
    };

    // キャッシュに保存
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&cache_path, &text).ok();

    Ok(text)
}

/// 検索キャッシュをクリア (強制リフレッシュ用)。
pub fn clear_search_cache(cache_root: &Path) {
    let dir = store_cache_dir(cache_root);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .map(|n| n.to_string_lossy().starts_with("search_"))
                .unwrap_or(false)
            {
                std::fs::remove_file(path).ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_github_repo_display() {
        let repo = GitHubRepo {
            full_name: "folke/snacks.nvim".to_string(),
            html_url: "https://github.com/folke/snacks.nvim".to_string(),
            description: Some("snacks".to_string()),
            stargazers_count: 1500,
            updated_at: "2026-04-14".to_string(),
            topics: vec![],
            default_branch: Some("main".to_string()),
        };
        assert_eq!(repo.plugin_name(), "snacks.nvim");
        assert_eq!(repo.stars_display(), "1.5k");
        assert!(repo.readme_url().contains("raw.githubusercontent.com"));
    }

    #[test]
    fn test_stars_display_under_1k() {
        let repo = GitHubRepo {
            full_name: "test/test".to_string(),
            html_url: String::new(),
            description: None,
            stargazers_count: 42,
            updated_at: String::new(),
            topics: vec![],
            default_branch: None,
        };
        assert_eq!(repo.stars_display(), "42");
    }

    #[test]
    fn test_cache_path_sanitizes_query() {
        let root = Path::new("/tmp/rvpm/nvim");
        let path = search_cache_path(root, "foo bar:baz");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("search_"));
        assert!(!name.contains(' '));
        assert!(!name.contains(':'));
    }

    #[test]
    fn test_store_cache_dir_uses_cache_root() {
        let root = Path::new("/custom/cache");
        assert_eq!(store_cache_dir(root), Path::new("/custom/cache/store"));
    }
}
