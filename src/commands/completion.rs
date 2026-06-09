use super::*;

/// `rvpm completion <SHELL>` — clap_complete に CLI 定義を渡して
/// stdout に補完スクリプトを書き出す (#114)。
///
/// 補完スクリプトの内容は CLI 定義 (Cli / Commands enum) から自動生成されるので、
/// サブコマンドや flag を追加した時点で自動的に反映される。 `rvpm.nvim` 側の
/// `lua/rvpm/command.lua` (Neovim 用 :Rvpm 補完) は別管理なので、 そちらは
/// CLAUDE.md のチェックリスト通り手動で sync する必要がある。
pub(crate) fn run_completion(shell: clap_complete::Shell) {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    let bin = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin, &mut std::io::stdout());
}
