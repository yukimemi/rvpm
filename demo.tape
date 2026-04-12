# rvpm demo — README GIF
# Usage: vhs demo.tape
# Note: On Windows, run from WSL (vhs hangs on Set Theme in native Windows)
# Note: FontFamily must be installed in the system. Adjust if tofu appears.

Output demo.gif
Set Shell "bash"
Set FontSize 16
Set FontFamily "PlemolJP Console NF"
Set Width 900
Set Height 600
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
Sleep 3s

# 2. Add plugins (one eager, one lazy+on_cmd, one lazy colorscheme)
Type "rvpm add folke/snacks.nvim"
Sleep 500ms
Enter
Sleep 3s

Type "rvpm add nvim-telescope/telescope.nvim --on-cmd Telescope"
Sleep 500ms
Enter
Sleep 3s

Type "rvpm add catppuccin/nvim --lazy true"
Sleep 500ms
Enter
Sleep 3s

# 3. List TUI
Type "rvpm list"
Sleep 500ms
Enter
Sleep 2s

# Navigate
Type "j"
Sleep 400ms
Type "j"
Sleep 400ms
Type "k"
Sleep 600ms

# Search
Type "/"
Sleep 500ms
Type "tele"
Sleep 1500ms
Enter
Sleep 800ms

# Help popup
Type "?"
Sleep 3s
Type "?"
Sleep 500ms

# Quit list
Type "q"
Sleep 1s
