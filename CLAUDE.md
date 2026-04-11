# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

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
```

## 設計原則

**必ず TDD で実装を進める。** テストを先に書いてから（失敗することを確認して）実装する。

**Resilience (障害耐性):** 1つのプラグインの失敗がシステム全体を止めてはならない。エラーは警告として報告し、処理を継続する。

## アーキテクチャ

### 全体構成

`src/main.rs` がエントリポイントかつコマンドハンドラ。各コマンドは `run_*()` 関数として実装され、Tokio の非同期ランタイム上で動作する。

```
src/
  main.rs    — CLI 定義(clap)、全コマンドの run_*() 実装、ヘルパー関数
  config.rs  — TOML 設定のパース (Tera テンプレート展開込み)
  git.rs     — git clone/pull/fetch/checkout の非同期ラッパー (Repo 構造体)
  link.rs    — merged ディレクトリへのリンク/ジャンクション作成
  loader.rs  — Neovim の loader.lua を生成するロジック
  tui.rs     — ratatui による進捗表示 TUI
```

### データフロー

1. `parse_config()` — TOML を読み込み、Tera テンプレートを展開してから `Config` 構造体にデシリアライズ
2. `sort_plugins()` — `depends` フィールドに基づいてトポロジカルソート（循環依存は警告のみ）
3. `run_sync()` — `JoinSet` + `Semaphore` で並列 git clone/pull → `merge_plugin()` で merged ディレクトリへリンク → `generate_loader()` で loader.lua 生成

### loader.lua の生成ロジック (`src/loader.rs`)

- `merge = true` のプラグインは `~/.cache/rvpm/merged/` に lua/plugin/after 等のサブディレクトリをリンクし、一括で RTP に追加
- `lazy = true` のプラグインは各トリガー（`on_cmd`, `on_ft`, `on_map`, `on_event`, `on_path`, `on_source`）に応じた Lua の autocmd/keymap を生成
- `load_lazy()` 関数はプラグインロード後に `vim.api.nvim_exec_autocmds("User", { pattern = "rvpm_loaded_" .. name })` を発火する（`on_source` の連鎖依存のため必須）
- `cond` フィールドは Lua 式として `if cond then ... end` でラップされる

### 並列実行と Semaphore

`run_sync()` と `run_update()` は `tokio::task::JoinSet` でタスクを並列スポーン。`config.options.concurrency` が設定されている場合、`tokio::sync::Semaphore` でタスク数を制限する。

```rust
let concurrency = resolve_concurrency(config.options.concurrency);
let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
// 各タスク内の冒頭:
let _permit = sem.acquire_owned().await.unwrap();
```

### TOML 設定のテンプレート

`parse_config()` は2段階でパースする: まず vars セクションのみ取り出し → Tera コンテキストに `vars`, `env`, `is_windows` を登録 → TOML 文字列全体をレンダリング → 最終パース。これにより `{{ vars.base }}` や `{{ env.HOME }}` が設定ファイル内で使える。

### Windows 対応

`src/link.rs` の `junction_or_symlink()` は `#[cfg(windows)]` で junction を使用し、シンボリックリンクの権限問題を回避する。

### CLI コマンド一覧

| コマンド | 関数 | 説明 |
|---------|------|------|
| `sync` | `run_sync()` | clone/pull + merged + loader.lua 生成 |
| `generate` | `run_generate()` | loader.lua のみ再生成 |
| `add <repo>` | `run_add()` | TOML 追加 + sync |
| `update [query]` | `run_update()` | 既存プラグインの pull (clone しない) |
| `remove [query]` | `run_remove()` | TOML + ディレクトリ削除 + generate |
| `edit [query]` | `run_edit()` | init/before/after.lua をエディタで編集 |
| `set [query]` | `run_set()` | lazy/merge 等を対話式に変更 |
| `clean` | `run_clean()` | 未使用リポジトリディレクトリを削除 |
| `status` | `run_status()` | 各プラグインの git 状態を表示 |
| `list` | `run_list()` | TUI でプラグイン一覧表示 |

### ディレクトリ規約

| パス | 用途 |
|------|------|
| `~/.config/rvpm/config.toml` | メイン設定ファイル |
| `~/.cache/rvpm/repos/<host>/<owner>/<repo>` | プラグインのクローン先 |
| `~/.cache/rvpm/merged/` | merge=true プラグインのリンク集約先 |
| `~/.cache/rvpm/loader.lua` | 生成された Neovim 用ローダー (デフォルト、`loader_path` で変更可) |
| `<config_root>/<host>/<owner>/<repo>/` | プラグインごとの init/before/after.lua |
