//! GitHub URL normalization helpers (#217).
//!
//! Parsing/formatting of plugin source URLs into the canonical `owner/repo`
//! form, duplicate detection (`installed_full_name` / `urls_match`), and the
//! `format_plugin_url` writer used by `rvpm add`. Extracted verbatim from the
//! monolithic lib.rs.

/// 入力 URL を GitHub の `owner/repo` 形式 (大文字小文字保持) に正規化する。
///
/// - `owner/repo` → `Some("owner/repo")`
/// - `https://github.com/Owner/Repo(.git)?(/)?` → `Some("Owner/Repo")`
/// - `git@github.com:Owner/Repo.git` → `Some("Owner/Repo")`
/// - `https://gitlab.com/...` 等の非 GitHub URL → `None`
///
/// 重複検出用 (`installed_full_name`) と config.toml 書き出し
/// (`format_plugin_url`) の両方で再利用する。大文字小文字は保持するので、
/// 比較目的では呼び出し側で `.to_lowercase()` すること。
pub(crate) fn github_owner_repo(url: &str) -> Option<String> {
    let trimmed = url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 2 {
            return Some(format!("{}/{}", parts[0], parts[1]));
        }
        return None;
    }
    for prefix in ["https://github.com/", "http://github.com/"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let parts: Vec<&str> = rest.split('/').collect();
            if parts.len() >= 2 {
                return Some(format!("{}/{}", parts[0], parts[1]));
            }
            return None;
        }
    }
    if trimmed.contains("://") {
        return None;
    }
    // スキーム無しの 2 セグメント形式は **owner/repo** のみを受理し、ローカル
    // パスは除外する。`./foo`, `../foo`, `~/foo`, `/foo`, `\foo`,
    // `C:/foo` (Windows drive letter) 等を GitHub shorthand と誤認しない
    // ようガードする。
    if looks_like_local_path(trimmed) {
        return None;
    }
    if trimmed.contains('/') && !trimmed.contains(' ') {
        let parts: Vec<&str> = trimmed.split('/').collect();
        if parts.len() == 2 && is_valid_github_owner(parts[0]) && is_valid_github_repo(parts[1]) {
            return Some(format!("{}/{}", parts[0], parts[1]));
        }
    }
    None
}

/// ローカルパスっぽい文字列を判定する (GitHub shorthand と区別するため)。
/// - 先頭が `./` / `../` / `~` / `/` / `\`
/// - Windows のドライブレター (`C:`, `D:` 等) で始まる
pub(crate) fn looks_like_local_path(s: &str) -> bool {
    if s.starts_with("./") || s.starts_with("../") {
        return true;
    }
    if s.starts_with('~') || s.starts_with('/') || s.starts_with('\\') {
        return true;
    }
    // Windows drive letter: `C:`, `d:` 等
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return true;
    }
    false
}

/// GitHub owner 名の許容文字集合 (alphanumeric と hyphen)。
/// 先頭 `-` は GitHub 側が拒否するので弾く。
pub(crate) fn is_valid_github_owner(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// GitHub repo 名の許容文字集合 (alphanumeric と `- _ .`)。
/// owner よりやや緩く、`.` や `_` を受ける。
pub(crate) fn is_valid_github_repo(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Plugin URL を GitHub の `owner/repo` 形式 (小文字) に正規化する。
/// GitHub の `full_name` は大文字小文字非依存なので lowercase で揃える。
/// - `"owner/repo"` → `Some("owner/repo")`
/// - `"https://github.com/Owner/Repo(.git)?"` → `Some("owner/repo")`
/// - `"git@github.com:Owner/Repo.git"` → `Some("owner/repo")`
/// - GitHub 以外 (gitlab 等) → None
pub(crate) fn installed_full_name(url: &str) -> Option<String> {
    github_owner_repo(url).map(|s| s.to_lowercase())
}

/// `config.toml` の url と lockfile / 他 plugin の url が同じリポジトリを
/// 指しているかを判定する。両方が GitHub URL として認識できれば正規化した
/// `owner/repo` で比較 (`owner/foo` と `https://github.com/owner/foo.git` を
/// 同一視)、そうでなければ生文字列で一致を見る。`rvpm add` の重複検出と同じ方針。
pub(crate) fn urls_match(a: &str, b: &str) -> bool {
    match (installed_full_name(a), installed_full_name(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// `rvpm add` が `config.toml` に書き込む URL を `options.url_style` に従って整形する。
/// GitHub リポジトリと認識できる入力は `Short` / `Full` で書き換え、
/// 認識できないもの (gitlab など) は入力をそのまま返す。
pub(crate) fn format_plugin_url(input: &str, style: crate::config::UrlStyle) -> String {
    use crate::config::UrlStyle;
    match github_owner_repo(input) {
        Some(owner_repo) => match style {
            UrlStyle::Short => owner_repo,
            UrlStyle::Full => format!("https://github.com/{}", owner_repo),
        },
        None => input.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_installed_full_name_owner_repo() {
        assert_eq!(
            installed_full_name("folke/snacks.nvim"),
            Some("folke/snacks.nvim".to_string())
        );
    }

    #[test]
    fn test_installed_full_name_https_url_with_git_suffix() {
        assert_eq!(
            installed_full_name("https://github.com/Owner/Repo.git"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_installed_full_name_https_url_without_git_suffix() {
        assert_eq!(
            installed_full_name("https://github.com/nvim-lua/plenary.nvim"),
            Some("nvim-lua/plenary.nvim".to_string())
        );
    }

    #[test]
    fn test_installed_full_name_ssh_url() {
        assert_eq!(
            installed_full_name("git@github.com:Owner/Repo.git"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_installed_full_name_non_github_returns_none() {
        assert_eq!(installed_full_name("https://gitlab.com/owner/repo"), None);
    }

    #[test]
    fn test_installed_full_name_case_normalized() {
        assert_eq!(
            installed_full_name("Folke/Snacks.NVIM"),
            Some("folke/snacks.nvim".to_string())
        );
    }

    // -------- urls_match --------

    #[test]
    fn test_urls_match_treats_github_short_and_full_as_equal() {
        // `owner/repo` 形式と `https://github.com/owner/repo` 形式は同一リポジトリ扱い。
        assert!(urls_match(
            "folke/snacks.nvim",
            "https://github.com/folke/snacks.nvim",
        ));
        assert!(urls_match(
            "https://github.com/Folke/Snacks.NVIM.git",
            "folke/snacks.nvim",
        ));
        assert!(urls_match(
            "git@github.com:folke/snacks.nvim.git",
            "folke/snacks.nvim",
        ));
    }

    #[test]
    fn test_urls_match_rejects_different_owner_or_repo() {
        // 同じ repo 名でも owner が違うのは別プラグイン。
        assert!(!urls_match("foo/snacks.nvim", "bar/snacks.nvim"));
        assert!(!urls_match("folke/snacks.nvim", "folke/other.nvim"));
    }

    #[test]
    fn test_urls_match_non_github_falls_back_to_string_eq() {
        // GitHub と認識できない URL は生文字列比較。
        assert!(urls_match(
            "https://gitlab.com/x/y",
            "https://gitlab.com/x/y",
        ));
        assert!(!urls_match(
            "https://gitlab.com/x/y",
            "https://gitlab.com/x/z",
        ));
    }

    #[test]
    fn test_installed_full_name_trailing_slash() {
        // `owner/repo/`, `.../repo.git/`, `.../repo/` をすべて許容する
        assert_eq!(
            installed_full_name("folke/snacks.nvim/"),
            Some("folke/snacks.nvim".to_string())
        );
        assert_eq!(
            installed_full_name("https://github.com/Owner/Repo/"),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            installed_full_name("https://github.com/Owner/Repo.git/"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_github_owner_repo_preserves_case() {
        // installed_full_name と違い case は保持する (config.toml 書き込み用途)
        assert_eq!(
            github_owner_repo("Folke/Snacks.NVIM"),
            Some("Folke/Snacks.NVIM".to_string())
        );
        assert_eq!(
            github_owner_repo("https://github.com/Owner/Repo.git"),
            Some("Owner/Repo".to_string())
        );
        assert_eq!(
            github_owner_repo("git@github.com:Owner/Repo.git"),
            Some("Owner/Repo".to_string())
        );
    }

    #[test]
    fn test_github_owner_repo_non_github_returns_none() {
        assert_eq!(github_owner_repo("https://gitlab.com/a/b"), None);
        assert_eq!(github_owner_repo("https://git.example/a/b"), None);
    }

    #[test]
    fn test_format_plugin_url_short_form() {
        use crate::config::UrlStyle;
        // 入力がどの形式でも owner/repo に統一
        assert_eq!(
            format_plugin_url("folke/snacks.nvim", UrlStyle::Short),
            "folke/snacks.nvim"
        );
        assert_eq!(
            format_plugin_url("https://github.com/folke/snacks.nvim", UrlStyle::Short),
            "folke/snacks.nvim"
        );
        assert_eq!(
            format_plugin_url("https://github.com/folke/snacks.nvim.git", UrlStyle::Short),
            "folke/snacks.nvim"
        );
    }

    #[test]
    fn test_format_plugin_url_full_form() {
        use crate::config::UrlStyle;
        // 入力がどの形式でも https://github.com/owner/repo に統一
        assert_eq!(
            format_plugin_url("folke/snacks.nvim", UrlStyle::Full),
            "https://github.com/folke/snacks.nvim"
        );
        assert_eq!(
            format_plugin_url("https://github.com/folke/snacks.nvim.git", UrlStyle::Full),
            "https://github.com/folke/snacks.nvim"
        );
        assert_eq!(
            format_plugin_url("git@github.com:folke/snacks.nvim.git", UrlStyle::Full),
            "https://github.com/folke/snacks.nvim"
        );
    }

    #[test]
    fn test_format_plugin_url_non_github_passthrough() {
        use crate::config::UrlStyle;
        // GitHub 以外は入力そのまま保存 (style 無視)
        assert_eq!(
            format_plugin_url("https://gitlab.com/g/h", UrlStyle::Short),
            "https://gitlab.com/g/h"
        );
        assert_eq!(
            format_plugin_url("https://gitlab.com/g/h", UrlStyle::Full),
            "https://gitlab.com/g/h"
        );
    }

    #[test]
    fn test_github_owner_repo_rejects_local_paths() {
        // `./foo`, `../foo`, `~/foo`, `/foo`, `\foo`, `C:/foo` を GitHub shorthand
        // と誤認しないこと (CodeRabbit major fix)。
        assert_eq!(github_owner_repo("./foo"), None);
        assert_eq!(github_owner_repo("../foo"), None);
        assert_eq!(github_owner_repo("~/foo"), None);
        assert_eq!(github_owner_repo("/tmp/foo"), None);
        assert_eq!(github_owner_repo("\\foo\\bar"), None);
        assert_eq!(github_owner_repo("C:/foo"), None);
        assert_eq!(github_owner_repo("d:/bar/baz"), None);
    }

    #[test]
    fn test_github_owner_repo_rejects_invalid_chars() {
        // owner / repo の文字集合を超えるものは GitHub shorthand と認めない。
        // owner は alphanumeric + `-`、repo は + `. _` まで許容。
        assert_eq!(github_owner_repo("foo bar/baz"), None);
        assert_eq!(github_owner_repo("-foo/bar"), None); // owner 先頭 `-`
        assert_eq!(github_owner_repo("foo!/bar"), None); // owner に `!`
    }

    #[test]
    fn test_github_owner_repo_accepts_normal_shorthand() {
        // 正常な owner/repo は従来どおり受理される
        assert_eq!(
            github_owner_repo("folke/snacks.nvim"),
            Some("folke/snacks.nvim".to_string())
        );
        assert_eq!(
            github_owner_repo("nvim-lua/plenary.nvim"),
            Some("nvim-lua/plenary.nvim".to_string())
        );
    }
}
