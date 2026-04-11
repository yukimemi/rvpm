# rvpm (Rust-based Vim Plugin Manager)

## コンセプト
- **Extremely Fast**: Rust の並行処理（Tokio）とマージ済みディレクトリによる爆速起動。
- **Type Safe & Robust**: 設定ファイル (TOML) ベースの堅牢な管理。
- **Convention over Configuration**: 規約に基づく設定ファイルの自動読み込み。
- **Hybrid CLI**: 強力な引数指定と、`skim` によるインタラクティブな操作の融合。

## 1. CLI サブコマンド
| コマンド | 引数/フラグ | 説明 |
| :--- | :--- | :--- |
| `add` | `<repo>` | プラグイン追加。TOML 更新 + `git clone` + `generate`。 |
| `set` | `[query] [opts]` | インタラクティブ/引数による設定変更。 |
| `edit` | `[query]` | `skim` で選択し、`init/before/after.lua` を編集。 |
| `update` | `[query]` | 指定（または全）プラグインを更新。並列実行。 |
| `remove` | `[query]` | プラグインを削除（TOML + ディレクトリ）。 |
| `sync` | - | TOML 状態を現実に反映（Clone/Clean/Generate）。 |
| `generate` | - | `loader.lua` を再生成。 |
| `clean` | - | 不要なプラグインディレクトリを物理削除。 |

## 2. TOML 設定スキーマ
```toml
[vars]
repo_base = "~/.cache/nvim/rvpm"
config_base = "~/.config/nvim/rc/after"

[options]
config_root = "{{ vars.config_base }}/plugins"
concurrency = 10
loader_path = "~/.cache/nvim/rvpm/loader.lua"

[[plugins]]
name = "plenary"
url = "nvim-lua/plenary.nvim"
merge = true  # Eager のデフォルト
lazy = false

[[plugins]]
name = "telescope"
url = "nvim-telescope/telescope.nvim"
lazy = true
# 豊富な遅延読み込みトリガー
on_cmd = ["Telescope"]
on_ft = ["rust", "toml"]
on_map = ["<leader>f"]
on_event = ["BufReadPre"]
on_path = ["*"]       # (予約)
on_source = ["plugin"]# (予約)
cond = "return true"  # (予約)
depends = ["plenary"]
# Git 参照の指定
# rev = "v0.1.0" # ブランチ、タグ、コミットハッシュのいずれかを指定
```

## 3. ディレクトリ規約
`options.config_root` 配下に `host/owner/repo` の階層で Lua 設定ファイルを配置。
例: `github.com/yukimemi/dvpm/`
- `init.lua`: RTP 追加前に実行。
- `before.lua`: RTP 追加直後に実行。
- `after.lua`: `plugin/*` source 後に実行。

## 4. 読み込み戦略
- **Merged View**: `merge = true` なプラグインは `~/.cache/rvpm/merged/` にリンク（Windows は Junction）され、一括で RTP に追加。
- **Static Loader**: Rust がファイル実在チェックを事前に行い、最小限の `dofile` のみを含む `loader.lua` を生成。
- **Windows Support**: シンボリックリンクの代わりにジャンクションを使用して権限問題を回避。

## 5. 設計原則
- **Resilience (障害耐性)**: 
    - 1つのプラグインの同期失敗や設定ミス（依存関係の欠如など）が、システム全体の実行や Neovim の起動を妨げてはならない。
    - エラーは警告として報告し、可能な限り後続の処理（`generate` 等）を継続する。
    - Neovim 起動時の安全性を最優先し、不完全な設定であっても最小限の起動を保証する。
