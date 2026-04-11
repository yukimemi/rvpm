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
on_cmd = ["Telescope"]
depends = ["plenary"]
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
