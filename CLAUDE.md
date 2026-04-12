# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## コンセプト

- **Extremely Fast**: Rust の並行処理 (Tokio) と merge 済みディレクトリ + 事前コンパイル済み loader.lua による爆速起動。
- **Type Safe & Robust**: TOML ベースの設定を serde で型付け、`resilience` 原則で 1 プラグインの失敗が全体を止めない。
- **Convention over Configuration**: `{config_root}/<host>/<owner>/<repo>/` 配下の `init.lua` / `before.lua` / `after.lua` を規約に従って自動読み込み。
- **Hybrid CLI**: 引数による一発操作 + `FuzzySelect` / TUI によるインタラクティブ操作を両立。
- **lazy.nvim を超える**: lazy.nvim と同じ `vim.go.loadplugins = false` 方式で完全制御しつつ、merge 最適化と generate 時の事前 glob で起動時 I/O を削減。

## Development Commands

```bash
# ビルド
cargo build

# テスト全実行
cargo test

# 単一テストの実行 (モジュール名::テスト関数名 で絞り込み)
cargo test test_generate_loader_with_cond
cargo test loader::tests
cargo test git::tests::test_git_update_method_pulls_latest

# リリースビルド
cargo build --release

# loader.lua の目視デバッグ (ignored test)
cargo test dump_full_sample_loader -- --ignored --nocapture
```

## 設計原則

**必ず TDD で実装を進める。** テストを先に書いてから (失敗することを確認して) 実装する。

**Resilience (障害耐性):** 1 つのプラグインの失敗がシステム全体を止めてはならない。同期失敗や設定ミス (依存関係の欠如など) は警告として報告し、可能な限り後続の処理 (`generate` 等) を継続する。Neovim 起動時の安全性を最優先し、不完全な設定であっても最小限の起動を保証する。

## TOML 設定スキーマ

```toml
[vars]
# ユーザー定義の変数。TOML 内 Tera テンプレートから {{ vars.xxx }} で参照できる。
repo_base   = "~/.cache/nvim/rvpm"
config_base = "~/.config/nvim/rc/after"

[options]
# per-plugin の init/before/after.lua を置くディレクトリの root
# 未指定なら ~/.config/rvpm/plugins (config.toml と隣合わせ)
config_root = "{{ vars.config_base }}/plugins"
# 並列数上限 (未指定なら無制限)
concurrency = 10
# rvpm のデータ置き場 root を上書き (未指定なら ~/.cache/rvpm)
# repos / merged / loader.lua 全部ここ配下にまとまる
# base_dir = "~/.cache/nvim/rvpm"
# loader.lua だけさらに細かく上書き (base_dir より優先)
loader_path = "~/.cache/nvim/rvpm/loader.lua"

[[plugins]]
name  = "plenary"
url   = "nvim-lua/plenary.nvim"
merge = true    # Eager のデフォルト (merged/ にリンク)
lazy  = false

[[plugins]]
name = "telescope"
url  = "nvim-telescope/telescope.nvim"
lazy = true
depends = ["plenary"]
# rev: ブランチ / タグ / コミットハッシュ
# rev = "v0.1.0"

# 遅延読み込みトリガー (全部省略可能、いずれか 1 つでも書けばその key で起動)
on_cmd    = ["Telescope"]                    # string | string[]
on_ft     = ["rust", "toml"]                 # string | string[]
on_event  = ["BufReadPre", "User LazyDone"]  # "User Xxx" は User イベント + pattern に展開される
on_path   = ["*.rs", "Cargo.toml"]           # BufRead/BufNewFile の glob
on_source = ["plenary"]                      # 他プラグインのロード完了 User イベントで起動
# on_map は string (単純) / table (mode + desc 指定) 混在可
on_map = [
  "<leader>f",                                              # mode = ["n"] (default)
  { lhs = "<leader>v",  mode = ["n", "x"] },
  { lhs = "<leader>g",  mode = ["n", "x"], desc = "Grep" },
]
# 条件付き読み込み (Lua 式)
cond = "vim.fn.has('win32') == 1"
```

## per-plugin 設定ファイル (config_root)

`options.config_root` 配下に `<host>/<owner>/<repo>/` の階層でプラグインごとの Lua 設定ファイルを配置できる。例: `~/.config/nvim/rc/after/plugins/github.com/nvim-telescope/telescope.nvim/`。

| ファイル | 実行タイミング | 典型用途 |
|---|---|---|
| `init.lua` | **RTP 追加前** (全プラグイン共通の pre-rtp phase) | `vim.g.xxx_setting = ...` 等の事前変数設定 |
| `before.lua` | **RTP 追加直後、`plugin/*` source 前** | setup を変える、lua/ モジュールの `require` など |
| `after.lua` | **`plugin/*` source 後** | plugin の関数を呼ぶ post-setup、keymap 設定 |

rvpm は generate 時に各ファイルの存在確認を行い、存在するものだけ `dofile(...)` を loader.lua に埋め込む (事前コンパイル)。

## アーキテクチャ

### 全体構成

`src/main.rs` がエントリポイントかつコマンドハンドラ。各コマンドは `run_*()` 関数として実装され、Tokio の非同期ランタイム上で動作する。

```
src/
  main.rs    — CLI 定義 (clap)、全コマンドの run_*() 実装、ヘルパー関数
  config.rs  — TOML 設定のパース (Tera テンプレート展開込み)、MapSpec 型、sort_plugins
  git.rs     — git clone/pull/fetch/checkout の非同期ラッパー (Repo 構造体)
  link.rs    — merged ディレクトリへのリンク / ジャンクション作成
  loader.rs  — Neovim の loader.lua を生成するロジック
  tui.rs     — ratatui による進捗・一覧表示 TUI
```

### データフロー

1. `parse_config()` — TOML を読み込み、Tera テンプレートを展開してから `Config` 構造体にデシリアライズ
2. `sort_plugins()` — `depends` フィールドに基づいてトポロジカルソート (循環依存は警告のみ)
3. `run_sync()` — `JoinSet` + `Semaphore` で並列 git clone/pull → `merge_plugin()` で merged ディレクトリへリンク → `build_plugin_scripts()` で事前 glob → `generate_loader()` で loader.lua 生成

### loader.lua の生成戦略 (`src/loader.rs`)

rvpm は **lazy.nvim 方式の完全制御** + **merge 最適化** + **generate 時の事前 glob** を全部取りしている。loader.lua の構造:

```
Phase 0:  vim.go.loadplugins = false          ← Neovim の auto-source 無効化
Phase 0.5: load_lazy helper 定義              ← lazy 用の実行時ローダー
Phase 1:  全プラグインの init.lua (依存順)    ← pre-rtp phase
Phase 2:  merged/ を rtp に 1 回 append       ← merge=true プラグインがあれば
Phase 3:  eager プラグインを依存順で処理:
            非 merge は vim.opt.rtp:append(plugin.path)
            before.lua
            plugin/**/*.{vim,lua} を事前 glob 済みファイル名で直接 source
            ftdetect/**/*.{vim,lua} を augroup filetypedetect 内で source
            after/plugin/**/*.{vim,lua} を source
            after.lua
            User autocmd "rvpm_loaded_<name>" 発火 (on_source チェーン用)
Phase 4:  lazy プラグインの trigger 登録      ← on_cmd / on_ft / on_map / on_event / on_path / on_source
```

重要な設計ポイント:

- `vim.go.loadplugins = false` で Neovim のデフォルト plugin loading を止めるため、loader.lua が全ての source を明示的に行う (lazy.nvim と同じ戦略)。これで二重 source が起きない。
- plugin 配下のファイル (`plugin/`, `ftdetect/`, `after/plugin/`) は **generate 時にディスクから walk** し、ファイルパスを loader.lua に直接埋め込む。起動時の `vim.fn.glob` 呼び出しゼロ → これが lazy.nvim より速い理由。
- `ftdetect/` は `augroup filetypedetect` 内で source しないと filetype 検出が正しく動かない (lazy.nvim も同様)。
- `load_lazy()` helper はプラグインロード後に `vim.api.nvim_exec_autocmds("User", { pattern = "rvpm_loaded_<name>" })` を発火する。これは `on_source` の連鎖依存のため必須。
- `cond` フィールドは Lua 式として `if cond then ... end` でラップされる。eager/lazy 両方で機能する。

### lazy trigger の実装

各トリガーは lazy.nvim の handler 実装を参考にしている。

| トリガー | 特徴 |
|---|---|
| `on_cmd` | `bang = true`, `range = true`, `nargs = "*"`, `complete` callback。callback 内で `event.bang / smods / fargs / range / count` を復元して `vim.cmd(cmd_table)` で dispatch。`:Foo!`, `:%Foo`, `:5Foo`, `:tab Foo` 全対応 |
| `on_ft` | ロード後に `exec_autocmds("FileType", { buffer = ev.buf })` で再発火 → 新しくロードされたプラグインの `ftplugin/<ft>.vim` が current buffer に対して発火する |
| `on_event` | `"User Xxx"` シンタックスで User event + pattern に展開。ロード後に `exec_autocmds(ev.event, { buffer, data })` で再発火 |
| `on_path` | `BufRead` / `BufNewFile` の glob パターン。同じく `exec_autocmds(ev.event, ...)` で再発火 |
| `on_map` | `vim.keymap.set({modes}, lhs, ..., { desc })`。MapSpec 型で `lhs + mode[] + desc` 対応。replay は `<Ignore>` prefix + feedkeys で安全化 (lazy.nvim と同パターン) |
| `on_source` | 他プラグインの `rvpm_loaded_<name>` User autocmd を受けて連鎖ロード |

on_map は `rhs` を spec に持たない設計 (lazy.nvim とは異なる)。理由:

- replay + after.lua の組み合わせで「プラグイン or ユーザーが最終的に set する keymap」を拾える (load_lazy 内で after.lua → feedkeys の順に走るため)
- プラグイン内部の keymap を静的解析するのは実用上困難
- `rhs` が必要な count / operator の edge case は `"m"` mode feedkeys でほぼカバー
- 将来必要になれば後方互換で `rhs` フィールドを足せる

ただし `mode` は必須概念: rvpm の stub keymap が install される mode と、最終的にユーザー/プラグインが set する keymap の mode が一致しないとトリガーが反応しない。デフォルトは `["n"]`。

### 並列実行と Semaphore

`run_sync()` と `run_update()` は `tokio::task::JoinSet` でタスクを並列スポーン。`config.options.concurrency` が設定されている場合、`tokio::sync::Semaphore` でタスク数を制限する。

```rust
let concurrency = resolve_concurrency(config.options.concurrency);
let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
// 各タスク内の冒頭:
let _permit = sem.acquire_owned().await.unwrap();
```

### TOML 設定のテンプレート

`parse_config()` は 2 段階でパースする: まず vars セクションのみ取り出し → Tera コンテキストに `vars`, `env`, `is_windows` を登録 → TOML 文字列全体をレンダリング → 最終パース。これにより `{{ vars.base }}` や `{{ env.HOME }}` が設定ファイル内で使える。

### 可変スキーマ (`string | string[]` / `MapSpec` / etc)

`config.rs` の `deserialize_string_or_vec` と `deserialize_map_specs` は `serde(untagged)` enum を使って柔軟な TOML 形式を受け付ける。

- `on_cmd = "Foo"` も `on_cmd = ["Foo", "Bar"]` も両方 OK
- `on_map = ["<leader>f"]` も `on_map = [{ lhs = "...", mode = ["n", "x"] }]` も両方 OK

書き込み側 (`set_plugin_list_field`) は 1 要素なら string、複数なら array で書き戻す (最小の表現)。

### Windows 対応

`src/link.rs` の `junction_or_symlink()` は `#[cfg(windows)]` で junction を使用し、シンボリックリンクの権限問題を回避する。

### パス規約 (固定 + 上書き可能)

設定/キャッシュは **全プラットフォーム共通で `~/.config/rvpm/` と `~/.cache/rvpm/` に固定**。Windows でも `dirs::config_dir()` (`%APPDATA%`) は使わない。理由:

- Neovim の慣習と揃う (`~/.config/nvim`)
- dotfiles を WSL / Linux / Windows で同じパス構造で共有できる
- 単一の mental model で済む

#### パスヘルパー (src/main.rs)

| ヘルパー | 用途 | 上書き方法 |
|---|---|---|
| `rvpm_config_path()` | `~/.config/rvpm/config.toml` | **固定** (chicken-and-egg 回避) |
| `resolve_base_dir(opt)` | `~/.cache/rvpm/` or `opt` の tilde 展開 | `options.base_dir` |
| `resolve_loader_path(opt, base_dir)` | `{base_dir}/loader.lua` or `opt` | `options.loader_path` |
| `resolve_config_root(opt)` | `~/.config/rvpm/plugins/` or `opt` | `options.config_root` |
| `expand_tilde(s)` | `~` / `~/...` / `~\...` を home dir に展開する汎用ヘルパー | — |

コード内で `.config/rvpm/...` や `.cache/rvpm/...` を文字列リテラルで直書きしないこと。必ずヘルパー経由。

#### 解決順序

- **base_dir**: `options.base_dir` (tilde 展開) → default `~/.cache/rvpm`
- **loader_path**: `options.loader_path` (tilde 展開) → `{base_dir}/loader.lua`
- **config_root**: `options.config_root` (tilde 展開) → `~/.config/rvpm/plugins`
- **repos**: 常に `{base_dir}/repos/<canonical>/` (plugin 単位の上書きは `plugin.dst`)
- **merged**: 常に `{base_dir}/merged/`

つまり `options.base_dir` だけ指定すれば repos / merged / loader.lua が全部連動する。`options.loader_path` は loader だけを別の場所に出したいレア要求向けの細かい上書き。`options.config_root` は per-plugin init/before/after.lua の置き場の上書きで、デフォルトは config.toml と隣合わせの `~/.config/rvpm/plugins/`。

### CLI コマンド一覧

| コマンド | 関数 | 説明 |
|---------|------|------|
| `sync [--prune]` | `run_sync()` | clone/pull + merged + loader.lua 生成。`--prune` で未使用プラグインディレクトリも削除。無指定でも未使用があれば末尾で警告表示 |
| `generate` | `run_generate()` | loader.lua のみ再生成 |
| `add <repo>` | `run_add()` | TOML 追加 + sync |
| `update [query]` | `run_update()` | 既存プラグインの pull (clone しない) |
| `remove [query]` | `run_remove()` | TOML + ディレクトリ削除 + generate |
| `edit [query] [--init\|--before\|--after]` | `run_edit()` | per-plugin init/before/after.lua をエディタで編集。フラグ指定でファイル選択をスキップ |
| `set [query] [flags]` | `run_set()` | lazy/merge/on_* などを対話式 or 引数で変更。`on_cmd` 等は comma-separated / JSON array 両対応、`--on-map` は JSON object/array で table 形式もサポート。`[ Open config.toml in $EDITOR ]` sentinel で TOML 直接編集に逃げられる |
| `config` | `run_config()` | `config.toml` を `$EDITOR` で直接開く (終了後に sync 実行) |
| `init [--write]` | `run_init()` | Neovim `init.lua` に loader.lua を繋ぐ `dofile(...)` スニペットを案内。`--write` で自動追記 (init.lua がなければ新規作成)。`$NVIM_APPNAME` を尊重 |
| `list [--no-tui]` | `run_list()` | プラグイン一覧表示。デフォルトは TUI で `[S] sync / [u/U] update / [g] generate / [d] remove / [e] edit / [s] set` のアクションキー対応。`--no-tui` で pipe-friendly な plain text 出力 (旧 `status` 相当) |

**廃止コマンド:**
- `status` → `list --no-tui` に統合 (plain text 出力で機能同等)
- `clean` → `sync --prune` に統合。未使用 dir がある状態で `sync` を走らせると末尾に警告が出るので発見しやすい

### ディレクトリレイアウト (デフォルト)

| パス | 用途 |
|------|------|
| `~/.config/rvpm/config.toml` | メイン設定ファイル (固定) |
| `~/.config/rvpm/plugins/<host>/<owner>/<repo>/` | per-plugin init/before/after.lua (`options.config_root` で上書き) |
| `~/.cache/rvpm/repos/<host>/<owner>/<repo>/` | プラグインのクローン先 |
| `~/.cache/rvpm/merged/` | merge=true プラグインのリンク集約先 |
| `~/.cache/rvpm/loader.lua` | 生成された Neovim 用ローダー |

`options.base_dir` を指定すると `~/.cache/rvpm/` 全体 (repos/merged/loader) が移動する。`options.loader_path` は loader.lua だけを個別に移動する。`options.config_root` は per-plugin 設定ディレクトリの置き場を個別に移動する。

### 初回導入サポート

`rvpm sync` / `rvpm generate` は末尾で `print_init_lua_hint_if_missing()` を呼び、`$NVIM_APPNAME` を考慮した Neovim `init.lua` が loader.lua を参照していない (or 未作成) 場合に案内を表示する。ユーザーは `rvpm init --write` を実行すると init.lua がなければ新規作成、あれば末尾追記 (冪等) してくれる。コメント付きで「これは rvpm が書き加えた」と分かる形で挿入される。
