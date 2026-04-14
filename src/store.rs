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

/// キャッシュディレクトリ。~/.cache/rvpm/<appname>/store/ に配置。
/// <appname> は $RVPM_APPNAME → $NVIM_APPNAME → "nvim" の順で決定。
fn store_cache_dir() -> PathBuf {
    let appname = std::env::var("RVPM_APPNAME")
        .or_else(|_| std::env::var("NVIM_APPNAME"))
        .unwrap_or_else(|_| "nvim".to_string());
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("rvpm")
        .join(appname)
        .join("store")
}

/// 検索結果のキャッシュパス。
fn search_cache_path(query: &str) -> PathBuf {
    let safe_name: String = query
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    store_cache_dir().join(format!("search_{}.json", safe_name))
}

/// README のキャッシュパス。
fn readme_cache_path(full_name: &str) -> PathBuf {
    let safe_name = full_name.replace('/', "__");
    store_cache_dir()
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

/// GitHub Search API でプラグインを検索。キャッシュがあればそれを返す。
pub fn search_plugins(query: &str) -> Result<Vec<GitHubRepo>> {
    // キャッシュチェック
    let cache_path = search_cache_path(query);
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

    let resp: SearchResponse = client
        .get("https://api.github.com/search/repositories")
        .query(&[
            ("q", search_query.as_str()),
            ("sort", "stars"),
            ("order", "desc"),
            ("per_page", "100"),
        ])
        .send()?
        .json()?;

    // キャッシュに保存
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_string(&resp.items)?;
    std::fs::write(&cache_path, json).ok();

    Ok(resp.items)
}

/// 人気プラグインのランキングを取得。
pub fn fetch_popular() -> Result<Vec<GitHubRepo>> {
    search_plugins("")
}

/// README を取得。キャッシュがあればそれを返す。
pub fn fetch_readme(repo: &GitHubRepo) -> Result<String> {
    let cache_path = readme_cache_path(&repo.full_name);
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
pub fn clear_search_cache() {
    let dir = store_cache_dir();
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
        let path = search_cache_path("foo bar:baz");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("search_"));
        assert!(!name.contains(' '));
        assert!(!name.contains(':'));
    }
}
