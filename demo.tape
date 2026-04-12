# rvpm demo — README GIF
# Usage: vhs demo.tape

Output demo.gif
Set Shell "bash"
Set FontSize 16
Set Width 1400
Set Height 750
Set Padding 20
Set TypingSpeed 50ms
Set Theme "Catppuccin Mocha"

# Clean slate
Hide
Type "rm -rf ~/.config/rvpm ~/.cache/rvpm ~/.config/nvim/init.lua 2>/dev/null; clear"
Enter
Sleep 500ms
Show

# 1. One-time setup
Sleep 500ms
Type "rvpm init --write"
Sleep 500ms
Enter
Sleep 2s

# 2. Add plugins
Type "rvpm add folke/snacks.nvim"
Sleep 500ms
Enter
Sleep 8s

Type "rvpm add nvim-telescope/telescope.nvim --on-cmd Telescope"
Sleep 500ms
Enter
Sleep 8s

Type "rvpm add catppuccin/nvim --lazy true"
Sleep 500ms
Enter
Sleep 8s

# 3. Show list TUI
Type "rvpm list"
Sleep 500ms
Enter
Sleep 2s

# Navigate
Sleep 500ms
Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Type "k"
Sleep 500ms

# Search
Type "/"
Sleep 500ms
Type "tele"
Sleep 1s
Enter
Sleep 500ms

# Help popup
Type "?"
Sleep 2s
Type "?"
Sleep 500ms

# Quit
Type "q"
Sleep 1s
