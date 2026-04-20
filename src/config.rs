use anyhow::Result;
use serde::Deserialize;
use tera::{Context, Tera};

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub vars: Option<serde_json::Value>,
    pub options: Options,
    #[serde(default)]
    pub plugins: Vec<Plugin>,
}

/// TUI で使用するアイコンスタイル。
#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum IconStyle {
    /// Nerd Font アイコン (デフォルト)
    #[default]
    Nerd,
    /// 標準 Unicode 記号 (○ ↻ ✓ ✗ 等)
    Unicode,
    /// ASCII のみ (. * + x 等)
    Ascii,
}

/// `rvpm add` が `config.toml` に書き込む URL の表記スタイル。
#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum UrlStyle {
    /// `owner/repo` (GitHub 省略形、デフォルト)
    #[default]
    Short,
    /// `https://github.com/owner/repo` (full URL)
    Full,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone)]
pub struct Options {
    /// per-plugin init/before/after.lua の置き場。
    /// 未指定なら `~/.config/rvpm/<appname>/plugins`。
    pub config_root: Option<String>,
    /// git 並列実行数 (default: 8)。
    pub concurrency: Option<usize>,
    /// rvpm のキャッシュ root。
    /// 未指定なら `~/.cache/rvpm/<appname>`。
    /// `repos/`, `merged/`, `loader.lua` がこの配下に配置される。
    pub cache_root: Option<String>,
    /// TUI アイコンスタイル: "nerd" (default), "unicode", "ascii"
    #[serde(default)]
    pub icons: IconStyle,
    /// chezmoi 連携を有効にするか。`true` なら rvpm が `config.toml` や
    /// per-plugin hook を書き換えた後に `chezmoi re-add` / `chezmoi add` を
    /// 自動実行して source 側へ同期する。`chezmoi` コマンドが無い環境では
    /// 静かにスキップ。デフォルト `false`。
    #[serde(default)]
    pub chezmoi: bool,
    /// `true` なら `sync` / `generate` 完了時に自動で `rvpm clean` 相当の処理
    /// (config.toml に無いプラグインディレクトリの削除) を実行する。
    /// `sync --prune` を毎回明示しなくてよくなる。デフォルト `false`。
    #[serde(default)]
    pub auto_clean: bool,
    /// `true` (デフォルト) なら `sync` / `generate` 完了時に `nvim --headless` を
    /// 起動して helptags を自動生成する。lazy プラグインは runtimepath に載らない
    /// ため、rvpm 側で対象 `doc/` ディレクトリを列挙して `:helptags <path>` を
    /// 個別実行する。`nvim` が PATH に無い場合は警告して skip (resilience)。
    #[serde(default = "default_auto_helptags")]
    pub auto_helptags: bool,
    /// `rvpm add` が `config.toml` に書き込む URL の形式。
    /// - `"short"` (デフォルト): `owner/repo`
    /// - `"full"`: `https://github.com/owner/repo`
    ///
    /// GitHub 以外の URL (gitlab 等) はこの設定に関わらずそのまま保存される。
    #[serde(default)]
    pub url_style: UrlStyle,
    /// `rvpm browse` の README preview 用オプション。
    #[serde(default)]
    pub browse: BrowseOptions,
    /// `rvpm sync` の fetch staleness window。humantime-lite 書式
    /// (`"6h" / "30m" / "1d" / "45s" / "0"`)。
    ///
    /// - 未指定 → デフォルト `"6h"`
    /// - `"0"` → cache 無効 (毎回 fetch、v3.18 以前の挙動)
    /// - 不正な値 → warn を出して `"6h"` にフォールバック (resilience)
    ///
    /// プラグイン単位で「前回 fetch から window 以内」なら sync 時の
    /// `git fetch` をスキップする。CLI からは `rvpm sync --refresh` / `--no-refresh`
    /// で上書き可能。
    pub fetch_interval: Option<String>,
}

impl Default for Options {
    /// `Options::default()` は serde の `#[serde(default = ...)]` と一致させる。
    /// 特に `auto_helptags` は serde では `true` がデフォルトなので、derive Default
    /// を使うと `false` になり parse 経由 vs 直接構築で挙動が分かれる。
    fn default() -> Self {
        Self {
            config_root: None,
            concurrency: None,
            cache_root: None,
            icons: IconStyle::default(),
            chezmoi: false,
            auto_clean: false,
            auto_helptags: default_auto_helptags(),
            url_style: UrlStyle::default(),
            browse: BrowseOptions::default(),
            fetch_interval: None,
        }
    }
}

/// `[options.browse]` 以下に置く、`rvpm browse` TUI 固有の設定。
#[derive(Debug, Deserialize, PartialEq, Eq, Default, Clone)]
pub struct BrowseOptions {
    /// README 表示を整形する外部コマンド。未設定/空なら内蔵 `tui-markdown`
    /// パイプラインを使う (offline fallback)。
    ///
    /// 受け渡し規約:
    /// - 生 README markdown は **stdin** に渡る。
    /// - コマンドの **stdout** を取り込んで、ANSI エスケープを
    ///   `ansi-to-tui` 経由で解釈して描画する。ANSI 対応の renderer
    ///   (`mdcat`, `glow -s dark`, `bat --language=markdown --color=always`
    ///   等) を想定。
    /// - 実行タイムアウトは 3 秒。超過した場合は fallback。
    /// - 各引数内で **Tera 風の `{{ name }}` 記法** の placeholder が展開される
    ///   (rvpm 他箇所の `[vars]` / テンプレートと統一、空白有無は任意):
    ///   - `{{ width }}` — README pane の内側幅 (列数)
    ///   - `{{ height }}` — 内側高さ
    ///   - `{{ file_path }}` — 生 markdown を書き出した tempfile 絶対パス
    ///     (使った場合 stdin 経由では渡さない)
    ///   - `{{ file_dir }}` — `{{ file_path }}` の親ディレクトリ
    ///   - `{{ file_name }}` — `{{ file_path }}` のファイル名部分
    ///   - `{{ file_stem }}` — `{{ file_name }}` から拡張子を除いた部分
    ///   - `{{ file_ext }}` — 拡張子 (dot 無し、例: `md`)
    ///
    /// 例:
    /// ```toml
    /// [options.browse]
    /// readme_command = ["mdcat"]
    /// # readme_command = ["mdcat", "--columns", "{{ width }}"]
    /// # readme_command = ["glow", "-s", "dark", "-w", "{{ width }}", "{{ file_path }}"]
    /// # readme_command = ["bat", "--language=markdown", "--color=always"]
    /// ```
    #[serde(default)]
    pub readme_command: Option<Vec<String>>,
}

/// Keymap 仕様. TOML では文字列 (`"<leader>f"`) またはテーブル
/// (`{ lhs = "<leader>f", mode = ["n", "x"], desc = "..." }`) で記述可能。
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct MapSpec {
    pub lhs: String,
    /// 空の場合は normal mode (`"n"`) として扱う。
    pub mode: Vec<String>,
    pub desc: Option<String>,
}

impl MapSpec {
    /// 空 mode を `["n"]` に正規化した Vec を返す。
    pub fn modes_or_default(&self) -> Vec<String> {
        if self.mode.is_empty() {
            vec!["n".to_string()]
        } else {
            self.mode.clone()
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq, Default, Clone)]
pub struct Plugin {
    pub name: Option<String>,
    pub url: String,
    pub dst: Option<String>,
    /// None = 未指定 (on_* があれば自動 true), Some(false) = 明示 eager, Some(true) = 明示 lazy
    #[serde(default, rename = "lazy")]
    pub lazy_raw: Option<bool>,
    /// parse 後に解決された値。コード上はこちらを参照する。
    #[serde(skip)]
    pub lazy: bool,
    #[serde(default = "default_merge")]
    pub merge: bool,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub on_cmd: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub on_ft: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_map_specs")]
    pub on_map: Option<Vec<MapSpec>>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub on_event: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub on_path: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub on_source: Option<Vec<String>>,
    pub depends: Option<Vec<String>>,
    pub build: Option<String>,
    pub rev: Option<String>,
    pub cond: Option<String>,
    /// dev = true のプラグインは sync/update をスキップする。
    /// ローカル開発中のプラグインに使う。
    #[serde(default)]
    pub dev: bool,
}

/// TOML 上で `on_cmd = "Foo"` (単一文字列) または `on_cmd = ["Foo", "Bar"]` (配列)
/// のどちらの形式でも受け付け、内部的には `Vec<String>` に正規化する。
fn deserialize_string_or_vec<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrVec {
        String(String),
        Vec(Vec<String>),
    }

    let opt = Option::<StringOrVec>::deserialize(deserializer)?;
    Ok(opt.map(|v| match v {
        StringOrVec::String(s) => vec![s],
        StringOrVec::Vec(v) => v,
    }))
}

/// Vec<String> を文字列形式 ("n") または配列形式 (["n", "x"]) で受ける。
fn deserialize_string_or_vec_vec<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SV {
        S(String),
        V(Vec<String>),
    }
    let sv: SV = Deserialize::deserialize(deserializer)?;
    Ok(match sv {
        SV::S(s) => vec![s],
        SV::V(v) => v,
    })
}

/// on_map の各要素を文字列 (`"<leader>f"`) またはテーブル (`{ lhs = "...", mode = ..., desc = "..." }`) で受け付ける。
fn deserialize_map_specs<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<MapSpec>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct MapSpecFull {
        lhs: String,
        #[serde(default, deserialize_with = "deserialize_string_or_vec_vec")]
        mode: Vec<String>,
        desc: Option<String>,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum MapSpecRaw {
        Simple(String),
        Full(MapSpecFull),
    }

    let opt = Option::<Vec<MapSpecRaw>>::deserialize(deserializer)?;
    Ok(opt.map(|v| {
        v.into_iter()
            .map(|raw| match raw {
                MapSpecRaw::Simple(lhs) => MapSpec {
                    lhs,
                    mode: Vec::new(),
                    desc: None,
                },
                MapSpecRaw::Full(full) => MapSpec {
                    lhs: full.lhs,
                    mode: full.mode,
                    desc: full.desc,
                },
            })
            .collect()
    }))
}

fn default_merge() -> bool {
    true
}

fn default_auto_helptags() -> bool {
    true
}

impl Plugin {
    pub fn canonical_path(&self) -> String {
        let url = self.url.trim_end_matches(".git");
        if url.contains("://") {
            let parts: Vec<&str> = url.split("://").collect();
            let path = parts[1];
            path.to_string()
        } else if url.contains("@") {
            let parts: Vec<&str> = url.split("@").collect();
            let path = parts[1].replace(":", "/");
            path.to_string()
        } else {
            // owner/repo 形式とみなす
            if url.contains("/") {
                format!("github.com/{}", url)
            } else {
                url.to_string()
            }
        }
    }

    /// URL からリポジトリ名を抽出してデフォルトの name として返す。
    /// `.git` suffix は除去。パスの最後のコンポーネントを使う。
    ///
    /// - `owner/repo`                       → `repo`
    /// - `https://github.com/owner/repo`    → `repo`
    /// - `https://github.com/owner/repo.git`→ `repo`
    /// - `git@github.com:owner/repo.git`    → `repo`
    pub fn default_name(&self) -> String {
        let url = self.url.trim_end_matches(".git");
        // SSH の `:` を `/` に正規化してからスラッシュで split
        let normalized = url.replace(':', "/");
        normalized.rsplit('/').next().unwrap_or(url).to_string()
    }

    /// `name` が明示されていればそれを、なければ `default_name()` を返す。
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.default_name())
    }
}

/// vars 内の相互参照を解決する最大反復回数。
const MAX_VARS_RESOLVE_ITERATIONS: usize = 10;

/// `options.browse.readme_command` 内で使える placeholder 名。`parse_config`
/// の Tera レンダリング時に自己射影 (value = `{{ name }}` リテラル) させ、
/// 実行時の `external_render::substitute` が展開するまで生き残らせる。
/// `src/external_render.rs` の `expand_args` と同期。
///
/// 注意: これらは **config.toml 全体の Tera context** に差し込まれるので、
/// `readme_command` 以外の文字列値 (例: `dst = "/tmp/{{ width }}"`) でも同じ
/// リテラルとして保持される。つまりここで挙げた名前については、未定義変数
/// エラーが出ない代わりに意図しない場所でリテラルとして残る可能性がある。
/// 実運用では `readme_command` 内以外で同じトークンを使う動機は薄いので妥協。
const README_COMMAND_PLACEHOLDERS: &[&str] = &[
    "width",
    "height",
    "file_path",
    "file_dir",
    "file_name",
    "file_stem",
    "file_ext",
];

/// [vars] セクションを TOML 全体パースなしでテキストから抽出する。
/// `{% if %}` 等の Tera 構文が含まれていても安全。
/// `[vars.sub]` のようなサブテーブルも正しく含める。
fn extract_vars_section(toml_str: &str) -> String {
    let mut in_vars = false;
    let mut vars_lines = vec!["[vars]".to_string()];
    for line in toml_str.lines() {
        let trimmed = line.trim();
        // [vars] セクション開始を検出 (空白やコメント付きにも対応)
        if !in_vars {
            let stripped = trimmed
                .split('#')
                .next()
                .unwrap_or("")
                .trim()
                .replace(' ', "");
            if stripped == "[vars]" {
                in_vars = true;
                continue;
            }
            continue;
        }
        // [vars] or [vars.xxx] のサブテーブルは含める
        // それ以外のセクション ([options], [[plugins]] 等) or Tera ブロックタグで終了
        if trimmed.starts_with('[') {
            let section_name = trimmed
                .split('#')
                .next()
                .unwrap_or("")
                .trim()
                .replace(' ', "");
            if section_name.starts_with("[vars.") || section_name.starts_with("[vars]") {
                vars_lines.push(line.to_string());
                continue;
            }
            break;
        }
        if trimmed.starts_with("{%") {
            break;
        }
        vars_lines.push(line.to_string());
    }
    vars_lines.join("\n")
}

pub fn parse_config(toml_str: &str) -> Result<Config> {
    // 1. [vars] セクションをテキストベースで抽出 (TOML パーサー不要)
    let vars_toml = extract_vars_section(toml_str);

    // 2. vars を TOML パースして初期値を取得
    #[derive(Deserialize)]
    struct VarsOnly {
        vars: Option<serde_json::Value>,
    }
    let vars_parsed: VarsOnly = toml::from_str(&vars_toml)
        .map_err(|e| anyhow::anyhow!("Failed to parse [vars] section: {}", e))?;

    // 3. env + is_windows を先に用意 (vars 内でも参照可能にする)
    let mut env_map = std::collections::HashMap::new();
    for (key, value) in std::env::vars() {
        env_map.insert(key, value);
    }

    // 4. vars 内の相互参照を反復レンダリングで解決
    let mut vars_value = vars_parsed
        .vars
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
    let mut converged = false;
    for _ in 0..MAX_VARS_RESOLVE_ITERATIONS {
        let mut ctx = Context::new();
        ctx.insert("vars", &vars_value);
        ctx.insert("env", &env_map);
        ctx.insert("is_windows", &cfg!(windows));
        let vars_str = serde_json::to_string(&vars_value)?;
        let rendered_str = Tera::one_off(&vars_str, &ctx, false)?;
        let new_value: serde_json::Value = serde_json::from_str(&rendered_str)?;
        if new_value == vars_value {
            converged = true;
            break;
        }
        vars_value = new_value;
    }
    if !converged {
        eprintln!(
            "Warning: [vars] cross-references did not converge after {} iterations",
            MAX_VARS_RESOLVE_ITERATIONS
        );
    }

    // 5. Tera Context の構築 (解決済み vars + env + is_windows)
    let mut context = Context::new();
    context.insert("vars", &vars_value);
    context.insert("is_windows", &cfg!(windows));
    context.insert("env", &env_map);

    // 6. `options.browse.readme_command` 用 placeholder (`{{ width }}` 等) を
    //    自己参照の literal 値として context に登録する。こうしないと Tera が
    //    未定義変数扱いで空文字列に置換してしまい、外部 renderer に
    //    `--columns ""` のような壊れた引数を渡してしまう。`{{ width }}` を
    //    評価した結果が `{{ width }}` になるように、リテラル値を自己射影する。
    for key in README_COMMAND_PLACEHOLDERS {
        let literal = format!("{{{{ {} }}}}", key);
        context.insert(*key, &literal);
    }

    // 7. 全体を Tera でレンダリング ({% if %} 等が動く)
    let rendered = Tera::one_off(toml_str, &context, false)?;

    // 8. 旧 `[options.store]` は v3.10 で `[options.browse]` に改名された。
    //    serde は未知のフィールドを黙って無視するので、そのままだと
    //    `readme_command` が無効になっていることに気付けない。明示的に
    //    warning を出してユーザーに migration を促す。
    if rendered.lines().any(|line| {
        line.split('#')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .collect::<String>()
            == "[options.store]"
    }) {
        eprintln!("\u{26a0} [options.store] is no longer supported; rename it to [options.browse]");
    }

    // 9. TOML パース
    let mut config: Config = toml::from_str(&rendered)?;

    // 9. lazy 自動解決: on_* トリガーがあれば lazy = true にする (明示 false は尊重)
    for plugin in config.plugins.iter_mut() {
        let has_trigger = plugin.on_cmd.is_some()
            || plugin.on_ft.is_some()
            || plugin.on_map.is_some()
            || plugin.on_event.is_some()
            || plugin.on_path.is_some()
            || plugin.on_source.is_some();
        plugin.lazy = match plugin.lazy_raw {
            Some(v) => v,                // 明示指定を尊重
            None if has_trigger => true, // トリガーあり → 自動 lazy
            None => false,               // トリガーなし → eager
        };
    }

    Ok(config)
}

/// `depends` の値は **URL でも display_name でも引ける** ように lookup する。
/// これにより `depends = ["plenary.nvim"]` (名前) でも `depends = ["nvim-lua/plenary.nvim"]`
/// (URL) でも同じプラグインを参照できる。`on_source` と同じ identifier 空間。
pub fn sort_plugins(plugins: &mut Vec<Plugin>) -> Result<()> {
    let mut sorted = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut visiting = std::collections::HashSet::new();

    // URL と display_name の両方で引けるマップを構築
    let mut plugin_map: std::collections::HashMap<String, &Plugin> =
        std::collections::HashMap::new();
    for p in plugins.iter() {
        plugin_map.insert(p.url.clone(), p);
        plugin_map.insert(p.display_name(), p);
    }

    fn visit(
        key: &str,
        plugin_map: &std::collections::HashMap<String, &Plugin>,
        visited: &mut std::collections::HashSet<String>,
        visiting: &mut std::collections::HashSet<String>,
        sorted: &mut Vec<Plugin>,
    ) -> Result<()> {
        if visited.contains(key) {
            return Ok(());
        }
        if visiting.contains(key) {
            eprintln!("Warning: Cyclic dependency detected: {}", key);
            return Ok(());
        }

        visiting.insert(key.to_string());

        if let Some(plugin) = plugin_map.get(key) {
            // 重複防止: URL で visited チェック (display_name 経由で同じプラグインを2回入れない)
            if visited.contains(&plugin.url) {
                visiting.remove(key);
                return Ok(());
            }
            if let Some(deps) = &plugin.depends {
                for dep in deps {
                    visit(dep, plugin_map, visited, visiting, sorted)?;
                }
            }
            visited.insert(plugin.url.clone());
            visited.insert(plugin.display_name());
            visiting.remove(key);
            sorted.push((*plugin).clone());
        } else {
            eprintln!("Warning: Dependency not found in config: {}", key);
            visited.insert(key.to_string());
            visiting.remove(key);
        }
        Ok(())
    }

    for plugin in plugins.iter() {
        visit(
            &plugin.url,
            &plugin_map,
            &mut visited,
            &mut visiting,
            &mut sorted,
        )?;
    }

    *plugins = sorted;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config_accepts_on_cmd_as_string() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_cmd = "Telescope"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(
            config.plugins[0].on_cmd,
            Some(vec!["Telescope".to_string()])
        );
    }

    #[test]
    fn test_parse_config_accepts_on_cmd_as_array() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_cmd = ["Telescope", "Grep"]
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(
            config.plugins[0].on_cmd,
            Some(vec!["Telescope".to_string(), "Grep".to_string()])
        );
    }

    #[test]
    fn test_parse_config_accepts_cache_root_option() {
        let toml = r#"
[options]
cache_root = "~/dotfiles/nvim/rvpm"

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(
            config.options.cache_root.as_deref(),
            Some("~/dotfiles/nvim/rvpm")
        );
    }

    #[test]
    fn test_parse_config_cache_root_defaults_to_none() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.options.cache_root, None);
    }

    #[test]
    fn test_parse_config_chezmoi_defaults_to_false() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert!(!config.options.chezmoi);
    }

    #[test]
    fn test_parse_config_auto_helptags_defaults_to_true() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert!(config.options.auto_helptags);
    }

    #[test]
    fn test_parse_config_accepts_auto_helptags_false() {
        let toml = r#"
[options]
auto_helptags = false

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert!(!config.options.auto_helptags);
    }

    #[test]
    fn test_parse_config_accepts_chezmoi_true() {
        let toml = r#"
[options]
chezmoi = true

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert!(config.options.chezmoi);
    }

    #[test]
    fn test_parse_config_preserves_readme_command_placeholders() {
        // `{{ width }}` 等は Tera パスで壊れず、リテラルのまま readme_command に残ること
        let toml = r#"
[options.browse]
readme_command = ["mdcat", "--columns", "{{ width }}", "{{ file_path }}"]

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(
            config.options.browse.readme_command,
            Some(vec![
                "mdcat".to_string(),
                "--columns".to_string(),
                "{{ width }}".to_string(),
                "{{ file_path }}".to_string(),
            ])
        );
    }

    #[test]
    fn test_parse_config_preserves_all_readme_command_placeholders() {
        let toml = r#"
[options.browse]
readme_command = [
  "r",
  "{{ width }}", "{{ height }}",
  "{{ file_path }}", "{{ file_dir }}",
  "{{ file_name }}", "{{ file_stem }}", "{{ file_ext }}",
]

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        let cmd = config.options.browse.readme_command.unwrap();
        assert!(cmd.contains(&"{{ width }}".to_string()));
        assert!(cmd.contains(&"{{ height }}".to_string()));
        assert!(cmd.contains(&"{{ file_path }}".to_string()));
        assert!(cmd.contains(&"{{ file_dir }}".to_string()));
        assert!(cmd.contains(&"{{ file_name }}".to_string()));
        assert!(cmd.contains(&"{{ file_stem }}".to_string()));
        assert!(cmd.contains(&"{{ file_ext }}".to_string()));
    }

    #[test]
    fn test_parse_config_icons_defaults_to_nerd() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.options.icons, IconStyle::Nerd);
    }

    #[test]
    fn test_parse_config_accepts_icons_unicode() {
        let toml = r#"
[options]
icons = "unicode"

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.options.icons, IconStyle::Unicode);
    }

    #[test]
    fn test_parse_config_accepts_icons_ascii() {
        let toml = r#"
[options]
icons = "ascii"

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.options.icons, IconStyle::Ascii);
    }

    #[test]
    fn test_parse_config_accepts_on_map_simple_string() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_map = ["<leader>f"]
"#;
        let config = parse_config(toml).unwrap();
        let maps = config.plugins[0].on_map.as_ref().unwrap();
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].lhs, "<leader>f");
        assert!(
            maps[0].mode.is_empty(),
            "simple form leaves mode empty (defaults to 'n' at generate)"
        );
        assert_eq!(maps[0].desc, None);
    }

    #[test]
    fn test_parse_config_accepts_on_map_table_form() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_map = [
  { lhs = "<leader>v", mode = ["n", "x"], desc = "visual thing" },
]
"#;
        let config = parse_config(toml).unwrap();
        let maps = config.plugins[0].on_map.as_ref().unwrap();
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].lhs, "<leader>v");
        assert_eq!(maps[0].mode, vec!["n".to_string(), "x".to_string()]);
        assert_eq!(maps[0].desc.as_deref(), Some("visual thing"));
    }

    #[test]
    fn test_parse_config_accepts_on_map_mixed_forms() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_map = [
  "<leader>a",
  { lhs = "<leader>b", mode = "x" },
  { lhs = "<leader>c", mode = ["n", "v"], desc = "C" },
]
"#;
        let config = parse_config(toml).unwrap();
        let maps = config.plugins[0].on_map.as_ref().unwrap();
        assert_eq!(maps.len(), 3);
        assert_eq!(maps[0].lhs, "<leader>a");
        assert!(maps[0].mode.is_empty());
        assert_eq!(maps[1].lhs, "<leader>b");
        assert_eq!(maps[1].mode, vec!["x".to_string()]);
        assert_eq!(maps[2].lhs, "<leader>c");
        assert_eq!(maps[2].mode, vec!["n".to_string(), "v".to_string()]);
        assert_eq!(maps[2].desc.as_deref(), Some("C"));
    }

    #[test]
    fn test_lazy_auto_true_when_on_cmd_set() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_cmd = "Foo"
"#;
        let config = parse_config(toml).unwrap();
        assert!(
            config.plugins[0].lazy,
            "on_cmd が設定されていれば lazy = true になるべき"
        );
    }

    #[test]
    fn test_lazy_auto_true_when_on_event_set() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_event = "BufReadPre"
"#;
        let config = parse_config(toml).unwrap();
        assert!(
            config.plugins[0].lazy,
            "on_event が設定されていれば lazy = true になるべき"
        );
    }

    #[test]
    fn test_lazy_auto_true_when_on_ft_set() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_ft = ["rust"]
"#;
        let config = parse_config(toml).unwrap();
        assert!(config.plugins[0].lazy);
    }

    #[test]
    fn test_lazy_auto_true_when_on_map_set() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_map = ["<leader>f"]
"#;
        let config = parse_config(toml).unwrap();
        assert!(config.plugins[0].lazy);
    }

    #[test]
    fn test_lazy_auto_true_when_on_source_set() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_source = "telescope.nvim"
"#;
        let config = parse_config(toml).unwrap();
        assert!(config.plugins[0].lazy);
    }

    #[test]
    fn test_lazy_auto_true_when_on_path_set() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_path = "*.rs"
"#;
        let config = parse_config(toml).unwrap();
        assert!(config.plugins[0].lazy);
    }

    #[test]
    fn test_lazy_explicit_false_overrides_auto() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
lazy = false
on_cmd = "Foo"
"#;
        let config = parse_config(toml).unwrap();
        assert!(
            !config.plugins[0].lazy,
            "lazy = false が明示されていればそちらを尊重"
        );
    }

    #[test]
    fn test_no_trigger_stays_eager() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert!(!config.plugins[0].lazy, "トリガーなしは eager のまま");
    }

    #[test]
    fn test_parse_config_accepts_on_event_as_string() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
on_event = "BufReadPre"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(
            config.plugins[0].on_event,
            Some(vec!["BufReadPre".to_string()])
        );
    }

    #[test]
    fn test_sort_plugins_dependencies() {
        let mut plugins = vec![
            Plugin {
                url: "A".to_string(),
                depends: Some(vec!["B".to_string()]),
                ..Default::default()
            },
            Plugin {
                url: "B".to_string(),
                ..Default::default()
            },
        ];

        sort_plugins(&mut plugins).unwrap();

        assert_eq!(plugins[0].url, "B");
        assert_eq!(plugins[1].url, "A");
    }

    #[test]
    fn test_sort_plugins_depends_by_display_name() {
        // depends に display_name (リポジトリ名) を使っても解決できる
        let mut plugins = vec![
            Plugin {
                url: "nvim-telescope/telescope.nvim".to_string(),
                depends: Some(vec!["plenary.nvim".to_string()]),
                ..Default::default()
            },
            Plugin {
                url: "nvim-lua/plenary.nvim".to_string(),
                ..Default::default()
            },
        ];

        sort_plugins(&mut plugins).unwrap();
        // plenary が先に来る
        assert_eq!(plugins[0].url, "nvim-lua/plenary.nvim");
        assert_eq!(plugins[1].url, "nvim-telescope/telescope.nvim");
    }

    #[test]
    fn test_sort_plugins_depends_by_url_still_works() {
        // depends に URL をそのまま使っても引ける (既存互換)
        let mut plugins = vec![
            Plugin {
                url: "nvim-telescope/telescope.nvim".to_string(),
                depends: Some(vec!["nvim-lua/plenary.nvim".to_string()]),
                ..Default::default()
            },
            Plugin {
                url: "nvim-lua/plenary.nvim".to_string(),
                ..Default::default()
            },
        ];

        sort_plugins(&mut plugins).unwrap();
        assert_eq!(plugins[0].url, "nvim-lua/plenary.nvim");
        assert_eq!(plugins[1].url, "nvim-telescope/telescope.nvim");
    }

    #[test]
    fn test_sort_plugins_cycle_resilience() {
        let mut plugins = vec![
            Plugin {
                url: "A".to_string(),
                depends: Some(vec!["B".to_string()]),
                ..Default::default()
            },
            Plugin {
                url: "B".to_string(),
                depends: Some(vec!["A".to_string()]),
                ..Default::default()
            },
            Plugin {
                url: "C".to_string(),
                ..Default::default()
            },
        ];

        let result = sort_plugins(&mut plugins);
        assert!(result.is_ok());
        assert!(plugins.iter().any(|p| p.url == "C"));
    }

    #[test]
    fn test_sort_plugins_missing_dependency_resilience() {
        let mut plugins = vec![Plugin {
            url: "A".to_string(),
            depends: Some(vec!["NOT_FOUND".to_string()]),
            ..Default::default()
        }];

        let result = sort_plugins(&mut plugins);
        // エラーにならずに成功すべき
        assert!(result.is_ok());
        // A はリストに残るべき
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].url, "A");
    }

    #[test]
    fn test_parse_config_with_tera() {
        let toml_content = r#"
[vars]
base = "/tmp/rvpm"

[options]

[[plugins]]
name = "plenary"
url = "nvim-lua/plenary.nvim"
dst = "{{ vars.base }}/plenary"
"#;

        let config = parse_config(toml_content).unwrap();
        assert_eq!(config.plugins[0].dst, Some("/tmp/rvpm/plenary".to_string()));
    }

    #[test]
    fn test_parse_config_with_env_and_os() {
        unsafe {
            std::env::set_var("RVPM_TEST_ENV", "hello");
        }
        let toml_content = r#"
[options]

[[plugins]]
name = "test"
url = "repo"
dst = "{{ env.RVPM_TEST_ENV }}_{{ is_windows }}"
"#;

        let config = parse_config(toml_content).unwrap();
        let expected_dst = format!("hello_{}", cfg!(windows));
        assert_eq!(config.plugins[0].dst, Some(expected_dst));
    }

    #[test]
    fn test_parse_complex_config() {
        let toml_content = r#"
[options]

[[plugins]]
url = "nvim-telescope/telescope.nvim"
lazy = true
on_cmd = ["Telescope"]
on_path = ["*.rs"]
on_source = ["plenary.nvim"]
depends = ["plenary"]
merge = false
"#;
        let config = parse_config(toml_content).unwrap();
        let p = &config.plugins[0];
        assert_eq!(p.url, "nvim-telescope/telescope.nvim");
        assert!(p.lazy);
        assert_eq!(p.on_cmd, Some(vec!["Telescope".to_string()]));
        assert_eq!(p.on_path, Some(vec!["*.rs".to_string()]));
        assert_eq!(p.on_source, Some(vec!["plenary.nvim".to_string()]));
        assert_eq!(p.depends, Some(vec!["plenary".to_string()]));
        assert!(!p.merge);
    }

    #[test]
    fn test_plugin_canonical_path() {
        let p1 = Plugin {
            url: "https://github.com/owner/repo".to_string(),
            ..Default::default()
        };
        assert_eq!(p1.canonical_path(), "github.com/owner/repo");

        let p2 = Plugin {
            url: "owner/repo".to_string(),
            ..Default::default()
        };
        assert_eq!(p2.canonical_path(), "github.com/owner/repo");

        let p3 = Plugin {
            url: "git@github.com:owner/repo.git".to_string(),
            ..Default::default()
        };
        assert_eq!(p3.canonical_path(), "github.com/owner/repo");
    }

    #[test]
    fn test_plugin_default_name() {
        // owner/repo shorthand
        let p1 = Plugin {
            url: "nvim-lua/plenary.nvim".to_string(),
            ..Default::default()
        };
        assert_eq!(p1.default_name(), "plenary.nvim");

        // Full HTTPS URL
        let p2 = Plugin {
            url: "https://github.com/yukimemi/chronicle.vim".to_string(),
            ..Default::default()
        };
        assert_eq!(p2.default_name(), "chronicle.vim");

        // Full URL with .git suffix
        let p3 = Plugin {
            url: "https://github.com/owner/repo.git".to_string(),
            ..Default::default()
        };
        assert_eq!(p3.default_name(), "repo");

        // SSH URL
        let p4 = Plugin {
            url: "git@github.com:owner/telescope.nvim.git".to_string(),
            ..Default::default()
        };
        assert_eq!(p4.default_name(), "telescope.nvim");

        // Explicit name overrides
        let p5 = Plugin {
            url: "https://github.com/owner/long-ugly-name".to_string(),
            name: Some("short".to_string()),
            ..Default::default()
        };
        assert_eq!(p5.display_name(), "short");

        // No explicit name → default_name
        assert_eq!(p1.display_name(), "plenary.nvim");
    }

    // ========================================================
    // Tera テンプレート拡張テスト
    // ========================================================

    #[test]
    fn test_tera_if_block_excludes_plugin() {
        let toml = r#"
[vars]
use_blink = false

[options]

[[plugins]]
url = "owner/always"

{% if vars.use_blink %}
[[plugins]]
url = "owner/blink"
{% endif %}
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.plugins.len(), 1);
        assert_eq!(config.plugins[0].url, "owner/always");
    }

    #[test]
    fn test_tera_if_block_includes_plugin_when_true() {
        let toml = r#"
[vars]
use_blink = true

[options]

[[plugins]]
url = "owner/always"

{% if vars.use_blink %}
[[plugins]]
url = "owner/blink"
{% endif %}
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.plugins.len(), 2);
    }

    #[test]
    fn test_vars_reference_other_vars() {
        let toml = r#"
[vars]
base = "/tmp"
full = "{{ vars.base }}/plugins"

[options]

[[plugins]]
url = "owner/repo"
dst = "{{ vars.full }}/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.plugins[0].dst, Some("/tmp/plugins/repo".to_string()));
    }

    #[test]
    fn test_vars_forward_reference() {
        let toml = r#"
[vars]
full = "{{ vars.base }}/plugins"
base = "/tmp"

[options]

[[plugins]]
url = "owner/repo"
dst = "{{ vars.full }}/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.plugins[0].dst, Some("/tmp/plugins/repo".to_string()));
    }

    #[test]
    fn test_tera_is_windows_in_if_block() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/always"

{% if is_windows %}
[[plugins]]
url = "owner/win-only"
{% endif %}
"#;
        let config = parse_config(toml).unwrap();
        if cfg!(windows) {
            assert_eq!(config.plugins.len(), 2);
        } else {
            assert_eq!(config.plugins.len(), 1);
        }
    }

    #[test]
    fn test_parse_config_dev_defaults_to_false() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert!(!config.plugins[0].dev);
    }

    #[test]
    fn test_parse_config_dev_option() {
        let toml = r#"
[options]

[[plugins]]
url = "owner/repo"
dev = true
dst = "~/src/owner/repo"
"#;
        let config = parse_config(toml).unwrap();
        assert!(config.plugins[0].dev);
    }
}
