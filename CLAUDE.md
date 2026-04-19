# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## コンセプト

- **Extremely Fast**: Rust の並行処理 (Tokio) と merge 済みディレクトリ + 事前コンパイル済み loader.lua による爆速起動。
- **Type Safe & Robust**: TOML ベースの設定を serde で型付け、`resilience` 原則で 1 プラグインの失敗が全体を止めない。
- **Convention over Configuration**: `{config_root}/<host>/<owner>/<repo>/` 配下の `init.lua` / `before.lua` / `after.lua` を規約に従って自動読み込み。
- **Hybrid CLI**: 引数による一発操作 + `FuzzySelect` / TUI によるインタラクティブ操作を両立。
- **Pre-compiled loader**: `vim.go.loadplugins = false` で Neovim の plugin loading を無効化し、generate 時に静的な loader.lua を生成。merge 最適化と事前 glob で起動時 I/O を削減。

## Git ワークフロー

- **main ブランチに直接 push しない。** 変更は必ずフィーチャーブランチを切り、Pull Request を作成する。
- 例外: `chore: bump version to ...` や `chore: release vX.Y.Z` のようなリリース関連 chore commit、および `git tag vX.Y.Z` の push は直接 main に push してよい (既存履歴もそのパターン)。
- ブランチ名は変更内容を端的に表す (例: `feat/add-only-sync-new-plugin`)。
- **PR のタイトル・本文は英語で書く。** コミットメッセージも英語。

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
nvim_rc = "~/.config/nvim/rc"

[options]
# per-plugin の init/before/after.lua を置くディレクトリの root
# 未指定なら ~/.config/rvpm/<appname>/plugins
config_root = "{{ vars.nvim_rc }}/plugins"
# 並列数上限 (デフォルト 8、GitHub rate limit 回避のため控えめ)
concurrency = 10
# config.toml から外したプラグインディレクトリを sync / generate 完了時に
# 自動削除 (デフォルト false)。毎回 `sync --prune` を指定する代わり。
# auto_clean = true
# sync / generate 完了時に nvim --headless で helptags を自動生成する
# (デフォルト true)。lazy プラグインは runtimepath に載らないため、rvpm 側で
# 対象 doc/ ディレクトリを列挙して :helptags <path> を個別実行する。
# auto_helptags = false
# `rvpm add` の URL 書き込み形式: "short" (owner/repo, デフォルト) か
# "full" (https://github.com/owner/repo)。重複検出は両形式を正規化して比較。
# url_style = "full"
# rvpm のデータ置き場 root を上書き (未指定なら ~/.cache/rvpm/<appname>)。
# repos / merged / loader.lua 全部 `{cache_root}/plugins/` 配下にまとまる。
# cache_root = "~/.cache/nvim/rvpm"

[options.browse]
# README 表示を外部コマンドに委譲する (browse TUI 専用)。
# stdin に raw markdown、stdout の ANSI エスケープを ansi-to-tui 経由で
# ratatui Text に変換。失敗/タイムアウト時は tui-markdown 内蔵パスに fallback。
# placeholder は Tera 風の `{{ name }}` 記法 (rvpm 他箇所と統一):
#   {{ width }} / {{ height }} / {{ file_path }} / {{ file_dir }}
#   {{ file_name }} / {{ file_stem }} / {{ file_ext }}
# readme_command = ["mdcat"]
# readme_command = ["glow", "-s", "dark", "-w", "{{ width }}", "{{ file_path }}"]

[[plugins]]
name  = "snacks"
url   = "folke/snacks.nvim"
# on_* なし → eager (起動時にロード)

[[plugins]]
name = "telescope"
url  = "nvim-telescope/telescope.nvim"
depends = ["snacks.nvim"]
# rev: ブランチ / タグ / コミットハッシュ
# rev = "v0.1.0"

# 遅延読み込みトリガー (いずれか 1 つでも書けば lazy は自動で true に推論される)
on_cmd    = ["Telescope"]                    # string | string[]
on_ft     = ["rust", "toml"]                 # string | string[]
on_event  = ["BufReadPre", "User LazyDone"]  # "User Xxx" は User イベント + pattern に展開される
on_path   = ["*.rs", "Cargo.toml"]           # BufRead/BufNewFile の glob
on_source = ["snacks.nvim"]                  # 他プラグインのロード完了 User イベントで起動 (display_name で指定)
# on_map は string (単純) / table (mode + desc 指定) 混在可
on_map = [
  "<leader>f",                                              # mode = ["n"] (default)
  { lhs = "<leader>v",  mode = ["n", "x"] },
  { lhs = "<leader>g",  mode = ["n", "x"], desc = "Grep" },
]
# 条件付き読み込み (Lua 式)
cond = "vim.fn.has('win32') == 1"
```

## グローバル hooks

`<config_root>/` 直下 (デフォルト `~/.config/rvpm/<appname>/`) に置くだけで自動適用される。設定ファイルへの記述不要 (Convention over Configuration)。

| ファイル | Phase | 実行タイミング |
|---|---|---|
| `<config_root>/before.lua` | 3 | `load_lazy` helper 定義後、全プラグインの `init.lua` より前 |
| `<config_root>/after.lua` | 9 | 全 lazy trigger 登録後 |

`<config_root>` は `options.config_root` 未指定時 `~/.config/rvpm/<appname>` (`<appname>` = `$RVPM_APPNAME` → `$NVIM_APPNAME` → `nvim`)。

`generate_loader()` は `LoaderOptions` 構造体 (`global_before: Option<PathBuf>`, `global_after: Option<PathBuf>`) を受け取り、ファイルが存在する場合だけ `dofile(...)` を埋め込む。

## per-plugin 設定ファイル (config_root)

`options.config_root` 配下に `<host>/<owner>/<repo>/` の階層でプラグインごとの Lua 設定ファイルを配置できる。例: `~/.config/nvim/rc/plugins/github.com/nvim-telescope/telescope.nvim/`。

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
  main.rs       — CLI 定義 (clap)、全コマンドの run_*() 実装、ヘルパー関数
  config.rs     — TOML 設定のパース (Tera テンプレート展開込み)、MapSpec 型、sort_plugins
  doctor.rs     — `rvpm doctor` — 17 診断 × 4 カテゴリ + render (nerd/unicode/ascii)
  git.rs        — git clone/pull/fetch/checkout の非同期ラッパー (Repo 構造体) + GitChange 記録
  helptags.rs   — nvim --headless で :helptags を実行して tags を生成
  link.rs       — merged ディレクトリへのファイル単位リンク (hard link、衝突 first-wins)、`placed` で勝者追跡用に新規配置ファイルを返す
  loader.rs     — Neovim の loader.lua を生成するロジック
  merge_conflicts.rs — `<cache_root>/merge_conflicts.json` の read/write (直近 sync 分のみ、doctor が読む)
  lockfile.rs   — `<config_root>/rvpm.lock` の read/write (reproducible plugin versions、dotfiles にコミットする前提)
  tui.rs        — ratatui による進捗・一覧表示 TUI
  update_log.rs — `<cache_root>/update_log.json` の read/append、BREAKING 判定、render
```

### データフロー

1. `parse_config()` — TOML を読み込み、Tera テンプレートを展開してから `Config` 構造体にデシリアライズ
2. `sort_plugins()` — `depends` フィールドに基づいてトポロジカルソート (循環依存は警告のみ)
3. `run_sync()` — `JoinSet` + `Semaphore` で並列 git clone/pull → `merge_plugin()` で merged ディレクトリへリンク → `build_plugin_scripts()` で事前 glob → `generate_loader()` で loader.lua 生成 (この中で eager→lazy 依存の昇格 pre-pass も実行) → `build_helptags()` で nvim --headless を起動して `:helptags` 実行 (options.auto_helptags=true 時のみ)

### loader.lua の生成戦略 (`src/loader.rs`)

rvpm は **plugin loading の完全制御** + **merge 最適化** + **generate 時の事前 glob** を行う。loader.lua の構造:

```
Pre-pass:  eager→lazy 依存昇格                ← eager プラグインが lazy dep を持つ場合、
                                               その dep を eager に昇格して stderr に警告
Phase 1:   vim.go.loadplugins = false          ← Neovim の auto-source 無効化
Phase 2:   load_lazy helper 定義               ← lazy 用の実行時ローダー (二重ロードガード付き)
Phase 3:   global before.lua                   ← <config_root>/before.lua (存在する場合)
Phase 4:   全プラグインの init.lua (依存順)   ← pre-rtp phase
Phase 5:   merged/ を rtp に 1 回 append       ← merge=true プラグインがあれば
Phase 6:   eager プラグインを依存順で処理:
             非 merge は vim.opt.rtp:append(plugin.path)
             before.lua
             plugin/**/*.{vim,lua} を事前 glob 済みファイル名で直接 source
             ftdetect/**/*.{vim,lua} を augroup filetypedetect 内で source
             after/plugin/**/*.{vim,lua} を source
             after.lua
             User autocmd "rvpm_loaded_<name>" 発火 (on_source チェーン用)
Phase 7:   lazy プラグインの trigger 登録     ← on_cmd / on_ft / on_map / on_event / on_path / on_source
             lazy→lazy 依存: トリガーコールバック内で dep を先行 load_lazy 呼び出し
Phase 8:   ColorSchemePre handler 登録        ← generate 時に colors/*.{vim,lua} が検出された
                                               lazy プラグインに自動登録。設定不要。
Phase 9:   global after.lua                   ← <config_root>/after.lua (存在する場合)
```

重要な設計ポイント:

- `vim.go.loadplugins = false` で Neovim のデフォルト plugin loading を止めるため、loader.lua が全ての source を明示的に行う。これで二重 source が起きない。
- plugin 配下のファイル (`plugin/`, `ftdetect/`, `after/plugin/`) は **generate 時にディスクから walk** し、ファイルパスを loader.lua に直接埋め込む。起動時の glob 呼び出しゼロ。
- `ftdetect/` は `augroup filetypedetect` 内で source しないと filetype 検出が正しく動かない。
- `load_lazy()` helper はプラグインロード後に `vim.api.nvim_exec_autocmds("User", { pattern = "rvpm_loaded_<name>" })` を発火する。これは `on_source` の連鎖依存のため必須。また `loaded["<name>"] = true` による二重ロードガードを内包する。
- `depends` フィールドはロード順序だけでなく**ロードそのもの**にも影響する: eager プラグインが lazy dep を参照する場合、generate 時の pre-pass でその dep を eager に昇格する (stderr に警告)。lazy プラグインが lazy dep を参照する場合、生成されるトリガーコールバック内で dep を先行 `load_lazy` 呼び出しする。
- `cond` フィールドは Lua 式として `if cond then ... end` でラップされる。eager/lazy 両方で機能する。
- **カラースキームの自動検出**: lazy プラグインのクローン先に `colors/*.{vim,lua}` が存在する場合、`generate_loader()` が generate 時にそのファイル名を走査し、`ColorSchemePre` autocmd ハンドラを phase 8 として自動生成する。設定ファイルへの追記は不要。eager プラグインは `colors/` が既に RTP 上にあるため影響を受けない。
- **denops プラグインの自動登録**: lazy プラグインのクローン先に `denops/<name>/main.{ts,js}` が存在する場合、`generate_loader()` が generate 時にそのパスを走査し、`load_lazy()` 呼び出しの末尾引数に `{ {"<name>", "<abs main>"}, ... }` を渡す。load_lazy 内で `pcall(vim.fn["denops#plugin#load"], name, script)` を発行するため、rtp append + plugin/* source 後に denops daemon へ明示登録される (denops.vim の auto-discover は VimEnter 一発のみで lazy load 後に rtp が拡張されても新しいプラグインを拾わないため、明示登録が必要)。denops.vim 本体が未ロードでも `pcall` で silently skip する。eager プラグインは VimEnter 時の denops discover が rtp 全走査するため手当て不要。

### update_log.json による変更履歴 (`src/update_log.rs`)

`sync` / `update` / `add` で git pull 完了直後に「変化があったプラグイン」を
`<cache_root>/update_log.json` へ append する。`rvpm log` はこれを読み出して
人間可読な digest を出力する。

スキーマ:
- `UpdateLog { runs: Vec<RunRecord> }`
- `RunRecord { timestamp, command, changes: Vec<ChangeRecord> }`
- `ChangeRecord { name, url, from, to, subjects, breaking_subjects, doc_files_changed }`

重要な設計:
- 履歴は **最大 20 runs** で cap (古いものから drop)。長大化しない。
- 書き込みは tempfile + atomic rename で race 耐性を確保。
- changes が空の run (pull したが HEAD 変わらず) は記録するが `rvpm log` では
  省略 (表示ノイズを減らすため)。
- **BREAKING 検出** は `is_breaking(subject, body) -> bool` の pure 関数で行う:
  - subject が `<type>!:` / `<type>(<scope>)!:` の Conventional Commits 形式
  - body / footer に `BREAKING CHANGE:` (case-insensitive) 行を含む
- **doc 変更検出** は `git diff --name-only <from>..<to> -- README* CHANGELOG* doc/`
  を subprocess で実行し、ファイル名リストを記録。patch 自体は記録せず、
  `rvpm log --diff` 実行時に `git diff` から都度取得 (容量爆発回避)。
- git 側の HEAD 取得 / commit walk / BREAKING 判定は `src/git.rs` の `Repo::sync`
  / `Repo::update` 内で gix を使用し、`Option<GitChange>` を返す。記録失敗 (disk full
  等) でも本処理は止めない (resilience)。

### rvpm.lock による再現性 (`src/lockfile.rs`)

`lazy.nvim` の `lazy-lock.json` と同じ思想。`<config_root>/rvpm.lock` に
プラグイン単位で pin した commit hash を記録し、dotfiles にコミットすることで
他マシン / fresh clone でも同じ commit 構成を再現できる。

スキーマ (TOML):
```toml
version = 1

[[plugins]]
name = "snacks.nvim"
url = "folke/snacks.nvim"
commit = "abc123..."
```

優先順位: **config の `rev` > lockfile の commit > 最新 HEAD**。config.toml に
`rev = "v1.2.3"` があるプラグインは explicit pin として最優先、次に lockfile の
commit、それも無ければ default branch の HEAD を pull する。

コマンド別の挙動:
- `rvpm sync`: lockfile を load → 各プラグインで上記優先順位で rev を決めて
  `gix_checkout` → sync 完了後の HEAD を upsert → 末尾で `retain_by_names` して
  config から外れたプラグインの entry を drop → atomic save。
- `rvpm sync --frozen`: sync 開始前に config の全 non-dev プラグインが lockfile に
  存在するかを確認。1 件でも欠けていれば即 `anyhow::bail!` — CI / fresh clone
  で strict reproducibility を要求するケース。
- `rvpm sync --no-lock`: lockfile の load/save 両方をスキップ。既存 dotfile の
  lockfile はそのまま残る (触らない)。
- `rvpm update [query]`: lockfile を checkout には**使わない** (常に pull latest)
  が、pull 完了後の新 HEAD で lockfile を上書き。部分 update (query 指定) でも
  対象外プラグインの entry は保持。
- `rvpm add <repo>`: 追加した 1 プラグインのみ lockfile に upsert + save。

実装上のポイント:
- `Repo::sync()` は no-op (HEAD 動かず) 時 `None` を返すので、lockfile 記録用に
  `Repo::head_commit()` を別途呼んで現 HEAD を取得 (fresh clone + no-op 両ケース
  で entry を確定させる)。
- `LockFile::save` 時に name で安定 sort → dotfile diff が最小化される。
- malformed / missing file は warn を stderr に流して empty LockFile に
  fallback (resilience)。
- `dev = true` プラグインは lockfile 対象外 (ローカル work in progress なので
  commit hash を pin する意味が無い)。
- `options.chezmoi = true` のとき、lockfile も config.toml / hook と同じく
  `chezmoi::write_path` + `chezmoi::apply` 経由で source 側に書いてから target に
  反映する。これをやらないと chezmoi の「source が truth」原則と衝突して、
  次回 `chezmoi apply` で古い lockfile に巻き戻る。
- `chezmoi::write_path` / `chezmoi::apply` は **async + 2 秒タイムアウト** で実装
  (`tokio::process::Command` + `tokio::time::timeout`)。`run_doctor` の外部コマンド
  probe と同じ思想で、壊れた PATH shim や応答しない subprocess で rvpm 全体が
  hang するのを防ぐ。`write_path` は `is_chezmoi_available` + 祖先を遡る複数の
  `chezmoi source-path` 全体に **単一の 2 秒 budget** をかぶせる (個別 timeout が
  累積して桁違いに膨らむのを防ぐため)。タイムアウト時は warn を stderr に流して
  target 側を返す (resilience)。

### helptags 自動生成 (`src/helptags.rs`)

`sync` / `generate` 完了時に `nvim --headless --clean -c "source <tmp.vim>" -c "qa!"` を 1 回起動して `:helptags <path>` を対象 `doc/` 全てに対して実行する。`options.auto_helptags = false` で無効化可。

loader.lua に組み込まない理由: rvpm のコンセプトは **Neovim 起動時の速度を最優先**。helptags 生成は nvim プロセス起動コストを伴うため、Neovim 起動時ではなく rvpm 側 (sync/generate) で事前実行する。

対象 `doc/` の列挙ルール (`collect_helptag_targets`):
- `merged_dir/doc/` が存在すれば最初に追加 — merge=true & !lazy プラグインの doc が 1 箇所にまとまるので `:helptags` 1 回で全部処理できる
- **lazy プラグインは merge=true でも個別追加が必要** — `main.rs:566-568` の条件で lazy は merged/ に入らないため、各プラグイン単体の `doc/` を処理する必要がある
- merge=false な eager プラグインも個別追加
- `cond` は Lua runtime 評価で Rust からは判定不可なので全プラグインを候補にする (= `rvpm list` に載っているもの = 対象)

コマンドライン引数長対策: Windows の `CreateProcess` 上限 (8KB 程度) に当たらないよう、`-c "helptags d1" -c "helptags d2" ...` を並べるのではなく、tempfile に Vim script (`try/catch` でラップ) を書き出して `-c "source <tmp>"` で一括 source する。

resilience: `nvim` が PATH に無ければ warn のみで rvpm 全体は続行。nvim プロセスが非 0 終了でも Ok を返す。`:helptags` の重複警告 (E154 など) は stderr にそのまま流す — merge を明示的に選んでいるユーザーへの改善シグナルとしての価値があるため抑制しない。

### lazy trigger の実装

各トリガーの実装:

| トリガー | 特徴 |
|---|---|
| `on_cmd` | `bang = true`, `range = true`, `nargs = "*"`, `complete` callback。callback 内で `event.bang / smods / fargs / range / count` を復元して `vim.cmd(cmd_table)` で dispatch。`:Foo!`, `:%Foo`, `:5Foo`, `:tab Foo` 全対応 |
| `on_ft` | ロード後に `exec_autocmds("FileType", { buffer = ev.buf })` で再発火 → 新しくロードされたプラグインの `ftplugin/<ft>.vim` が current buffer に対して発火する |
| `on_event` | `"User Xxx"` シンタックスで User event + pattern に展開。ロード後に `exec_autocmds(ev.event, { buffer, data })` で再発火 |
| `on_path` | `BufRead` / `BufNewFile` の glob パターン。同じく `exec_autocmds(ev.event, ...)` で再発火 |
| `on_map` | `vim.keymap.set({modes}, lhs, ..., { desc })`。MapSpec 型で `lhs + mode[] + desc` 対応。replay は `<Ignore>` prefix + feedkeys で安全化 |
| `on_source` | 他プラグインの `rvpm_loaded_<name>` User autocmd を受けて連鎖ロード |

on_map は `rhs` を spec に持たない設計。理由:

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

### merge 戦略 (`src/link.rs`)

`merge_plugin()` は **ファイル単位** で merged ディレクトリにリンクする。設計のポイント:

- **ファイルは hard link** で張る (Windows でも管理者権限不要、Unix でも安定)。同一ボリューム必須だが repos / merged が同じ `<cache_root>` 配下なので OK。別ボリューム等で hard link が失敗したら `std::fs::copy` にフォールバック。junction はディレクトリ専用なのでファイルには使えない。symbolic link は Windows で admin 権限が要るので不採用
- **ディレクトリは作るだけ** (`create_dir_all`)。ディレクトリ自体は実体を作り、その中身をファイル単位で再帰してリンクする。ディレクトリを junction で張る方式 (旧実装) だと、複数 plugin が同じ階層下にファイルを置くケース (例: 複数の cmp 系 plugin が `lua/cmp/` を共有) で後勝ち上書きになり前の内容が消える
- **first-wins + 衝突サマリ** — 衝突したら新しい方を skip して `MergeConflict { relative }` を集める。`MergeResult.placed` は今回新規配置したファイルリストを返し、main.rs 側で `HashMap<PathBuf, String>` を維持して **勝者 plugin 名を lookup** する (loser だけだと「誰と被ったの？」が分からないため)。`run_sync` / `run_generate` の末尾で `print_merge_conflicts` がプラグインごとにグループ化し、各行に `(kept: <winner>)` を付けて stderr 表示 + `<cache_root>/merge_conflicts.json` に毎回上書き保存。`rvpm doctor` が後者を読み出して warn にする
- **plugin ルート直下のファイルは無視** — README.md / LICENSE / Makefile / package.json / *.toml 等のメタファイルは rtp に置く意味が無く、plugin 横断で同名衝突するだけのノイズ
- **plugin ルート直下のディレクトリは rtp 慣習 + denops のみ allowlist** — `plugin/`, `lua/`, `doc/`, `ftplugin/`, `ftdetect/`, `syntax/`, `indent/`, `colors/`, `compiler/`, `autoload/`, `after/`, `queries/`, `parser/`, `rplugin/`, `spell/`, `keymap/`, `lang/`, `pack/`, `tutor/` (`:Tutor` 用)、`denops/` (denops.vim の TypeScript plugin 用)。`tests/` `scripts/` `examples/` `src/` 等は rtp 無関係なので除外
- **全階層で dotfile (`.gitignore`, `.luarc.json`, `.editorconfig`, `.gitkeep` 等) を skip** — Neovim 起動に無関係で、`doc/.gitignore` のように深い階層でも plugin 横断で名前が被って衝突警告のノイズになる

### Windows 対応

merge 戦略がファイル単位 hard link に切り替わって以降、Windows でも管理者権限要らず・junction も symbolic link も使わない構成になった。`std::fs::hard_link` は NTFS 上で動き、admin 不要。ディレクトリは `create_dir_all` で実体を作るので junction は不要。シンボリックリンクの権限問題は回避済み。

### パス規約 (固定 + 上書き可能)

設定/キャッシュは **全プラットフォーム共通で `~/.config/rvpm/` と `~/.cache/rvpm/` に固定**。Windows でも `dirs::config_dir()` (`%APPDATA%`) は使わない。理由:

- Neovim の慣習と揃う (`~/.config/nvim`)
- dotfiles を WSL / Linux / Windows で同じパス構造で共有できる
- 単一の mental model で済む

#### パスヘルパー (src/main.rs)

| ヘルパー | 用途 | 上書き方法 |
|---|---|---|
| `rvpm_config_path()` | `~/.config/rvpm/config.toml` | **固定** (chicken-and-egg 回避) |
| `resolve_cache_root(opt)` | `~/.cache/rvpm/<appname>` or `opt` の tilde 展開 | `options.cache_root` |
| `resolve_repos_dir(cache_root)` | `{cache_root}/plugins/repos` | — |
| `resolve_merged_dir(cache_root)` | `{cache_root}/plugins/merged` | — |
| `resolve_loader_path(cache_root)` | `{cache_root}/plugins/loader.lua` | — |
| `resolve_config_root(opt)` | `~/.config/rvpm/<appname>/plugins/` or `opt` | `options.config_root` |
| `expand_tilde(s)` | `~` / `~/...` / `~\...` を home dir に展開する汎用ヘルパー | — |

コード内で `.config/rvpm/...` や `.cache/rvpm/...` を文字列リテラルで直書きしないこと。必ずヘルパー経由。

#### 解決順序

- **cache_root**: `options.cache_root` (tilde 展開) → default `~/.cache/rvpm/<appname>`
- **config_root**: `options.config_root` (tilde 展開) → `~/.config/rvpm/<appname>/plugins`
- **repos**: 常に `{cache_root}/plugins/repos/<canonical>/` (plugin 単位の上書きは `plugin.dst`)
- **merged**: 常に `{cache_root}/plugins/merged/`
- **loader**: 常に `{cache_root}/plugins/loader.lua`

つまり `options.cache_root` だけ指定すれば repos / merged / loader.lua が全部連動する。`options.config_root` は per-plugin init/before/after.lua の置き場の上書きで、デフォルトは config.toml と隣合わせの `~/.config/rvpm/<appname>/plugins/`。

### CLI コマンド一覧

| コマンド | 関数 | 説明 |
|---------|------|------|
| `sync [--prune] [--frozen] [--no-lock]` | `run_sync()` | clone/pull + merged + loader.lua 生成。`--prune` で未使用プラグインディレクトリも削除。無指定でも未使用があれば末尾で警告表示。lockfile (`<config_root>/rvpm.lock`) を読み込んで pin された commit に寄せ、sync 完了後に新 HEAD を書き戻す。`--frozen` で未登録プラグインがあれば即エラー (CI / fresh machine)、`--no-lock` で完全スキップ |
| `generate` | `run_generate()` | loader.lua のみ再生成 |
| `clean` | `run_clean()` | config.toml から外したプラグインの `{cache_root}/plugins/repos/<host>/<owner>/<repo>/` を削除。git 操作なしで `sync --prune` より高速 (200+ プラグインでの所要時間対策)。共有ヘルパー `prune_unused_repos()` を `sync --prune` と共用 |
| `add <repo>` | `run_add()` | TOML 追加 + 当該プラグインだけ clone + generate。重複検出は `installed_full_name` で正規化 (https / owner/repo / ssh / 大文字小文字 / `.git` / 末尾 `/` の揺れを吸収し、`rvpm browse` の installed マークと同じロジックを共用)。書き込み URL 形式は `options.url_style` (`short` / `full`) に従う |
| `update [query]` | `run_update()` | 既存プラグインの pull (clone しない)。完了後に lockfile を新 HEAD で上書き (部分 update 時も対象外の entry は残す) |
| `remove [query]` | `run_remove()` | TOML + ディレクトリ削除 + generate |
| `edit [query] [--init\|--before\|--after] [--global]` | `run_edit()` | per-plugin init/before/after.lua をエディタで編集。フラグ指定でファイル選択をスキップ。`--global` で global hooks (`<config_root>/before.lua` / `after.lua`) を編集。インタラクティブ選択時は `[ Global hooks ]` sentinel でも同じ動作 |
| `set [query] [flags]` | `run_set()` | lazy/merge/on_* などを対話式 or 引数で変更。`on_cmd` 等は comma-separated / JSON array 両対応、`--on-map` は JSON object/array で table 形式もサポート。`[ Open config.toml in $EDITOR ]` sentinel で TOML 直接編集に逃げられる |
| `config` | `run_config()` | `config.toml` を `$EDITOR` で直接開く (終了後に generate のみ実行。新規プラグイン追加した場合は `rvpm sync` 明示実行) |
| `init [--write]` | `run_init()` | Neovim `init.lua` に loader.lua を繋ぐ `dofile(...)` スニペットを案内。`--write` で自動追記 (init.lua がなければ新規作成)。`$NVIM_APPNAME` を尊重 |
| `list [--no-tui]` | `run_list()` | プラグイン一覧表示。デフォルトは TUI で `[S] sync / [u/U] update / [d] remove / [e] edit / [s] set / [c] config.toml / [b] browse / [?] help` のアクションキー対応。ナビ: `j/k/g/G/Ctrl-d/u/f/b`、検索: `/n/N`。`--no-tui` で pipe-friendly な plain text 出力 |
| `browse` | `run_browse()` | GitHub `neovim-plugin` トピックのプラグインブラウザ TUI (最大 300 件、3 ページ取得)。README は tui-markdown で GFM レンダ (`options.browse.readme_command` を設定すれば外部 renderer = mdcat / glow 等に委譲可、fallback 込み)。行頭の `✓` でインストール済みを表示し、`Enter` で installed plugin は警告して add をスキップ。`/` ローカルインクリメンタル検索 (name + description + topics) + `n`/`N` でマッチジャンプ。`S` で GitHub API 検索。`Tab` でリスト/README フォーカス切替。`o` でブラウザ、`s` でソート切替、`R` でキャッシュクリア+再取得、`c` で config.toml をエディタで開く、`l` で list TUI に遷移、`?` でヘルプ |
| `doctor` | `run_doctor()` | 設定 / 状態 / Neovim 連携 / 外部ツールの 16 項目を診断する one-shot コマンド。4 カテゴリ (plugin config / state integrity / Neovim integration / external tools)、出力は `options.icons` (nerd/unicode/ascii) に従う。exit code: `0` = 全 ok、`1` = error あり、`2` = warn のみ。外部コマンド (nvim/git/chezmoi) の `--version` 実行は `tokio::process::Command` + 2s timeout で hang しない |
| `log [query] [--last N] [--full] [--diff]` | `run_log()` | `sync` / `update` / `add` 実行時に記録される変更履歴 (`<cache_root>/update_log.json`) を表示。`[query]` で plugin 名部分一致フィルタ、`--last N` (default 1、max 20) で直近 N 回の run 表示、`--diff` で README / CHANGELOG / doc/ の patch 埋め込み、`--full` は将来の body 表示用予約。Conventional Commits の `<type>!:` / `BREAKING CHANGE:` footer は `⚠ BREAKING` プレフィックスで強調 |

**廃止コマンド:**
- `status` → `list --no-tui` に統合 (plain text 出力で機能同等)

### ディレクトリレイアウト (デフォルト)

| パス | 用途 |
|------|------|
| `~/.config/rvpm/config.toml` | メイン設定ファイル (**appname に関わらず固定** — chicken-and-egg 回避のため) |
| `~/.config/rvpm/<appname>/before.lua` | グローバル before hook (phase 3、全 init.lua より前。存在すれば自動適用) |
| `~/.config/rvpm/<appname>/after.lua` | グローバル after hook (phase 9、全 lazy trigger 登録後。存在すれば自動適用) |
| `~/.config/rvpm/<appname>/plugins/<host>/<owner>/<repo>/` | per-plugin init/before/after.lua (`options.config_root` で上書き) |
| `~/.config/rvpm/<appname>/rvpm.lock` | プラグイン commit pin の lockfile (`options.config_root` で上書き)。dotfiles にコミットして他マシンで再現する |
| `~/.cache/rvpm/<appname>/plugins/repos/<host>/<owner>/<repo>/` | プラグインのクローン先 |
| `~/.cache/rvpm/<appname>/plugins/merged/` | merge=true プラグインのリンク集約先 |
| `~/.cache/rvpm/<appname>/plugins/loader.lua` | 生成された Neovim 用ローダー |
| `~/.cache/rvpm/<appname>/plugins/merged/doc/tags` | `:helptags` で生成された merge=true プラグインの統合 tags |
| `~/.cache/rvpm/<appname>/plugins/repos/<host>/<owner>/<repo>/doc/tags` | lazy / merge=false プラグインの個別 tags |
| `~/.cache/rvpm/<appname>/update_log.json` | `sync` / `update` / `add` 実行時の変更履歴 (`rvpm log` が読み出す、最大 20 runs) |
| `~/.cache/rvpm/<appname>/merge_conflicts.json` | 直近 `sync` / `generate` の merge 衝突 snapshot (`rvpm doctor` が読み出す)。履歴ではなく毎回上書き |

`<appname>` は `$RVPM_APPNAME` → `$NVIM_APPNAME` → `"nvim"` の順で決まる。`options.cache_root` を指定すると `~/.cache/rvpm/<appname>/` 全体 (repos/merged/loader.lua) が移動する。`options.config_root` は per-plugin 設定ディレクトリの置き場を個別に移動する。

### 初回導入サポート

`rvpm sync` / `rvpm generate` は末尾で `print_init_lua_hint_if_missing()` を呼び、`$NVIM_APPNAME` を考慮した Neovim `init.lua` が loader.lua を参照していない (or 未作成) 場合に案内を表示する。ユーザーは `rvpm init --write` を実行すると init.lua がなければ新規作成、あれば末尾追記 (冪等) してくれる。コメント付きで「これは rvpm が書き加えた」と分かる形で挿入される。
