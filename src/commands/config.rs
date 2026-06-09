use super::*;

/// `rvpm config` — config.toml を $EDITOR で直接開く。
/// ファイルが無ければテンプレートで自動作成してから開く。
/// 編集前後の mtime を比較して、 **実際に変更があった場合のみ `Ok(true)` を返す**。
/// 呼び出し側 (rvpm list TUI の `c` キー等) は戻り値で sync / generate を条件実行する。
pub(crate) async fn run_config() -> Result<bool> {
    let config_path = rvpm_config_path();
    ensure_config_exists(&config_path)?;
    let chezmoi_enabled = read_chezmoi_flag(&config_path);
    let edit_target = chezmoi::write_path(chezmoi_enabled, &config_path).await;
    println!("Opening {}", edit_target.display());
    // mtime を編集前後で比較して、 config.toml に変更が無ければ caller (rvpm list
    // TUI 等) が後続の `run_generate` を skip できるようにする。 todoke 系の
    // 「open & close せず別 nvim instance に send」ワークフローで、 編集してない
    // のに毎回 generate が走ると view tree の rebuild race を引き起こす #119。
    let before_mtime = std::fs::metadata(&edit_target)
        .and_then(|m| m.modified())
        .ok();
    open_editor_at_line(&edit_target, 1)?;
    let after_mtime = std::fs::metadata(&edit_target)
        .and_then(|m| m.modified())
        .ok();
    chezmoi::apply(&edit_target, &config_path).await;
    Ok(before_mtime != after_mtime)
}
