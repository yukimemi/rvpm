# rvpm browse TUI demo — README GIF
# Usage: (cd vhs && vhs browse.tape)   # output lands next to this tape
# Note: On Windows, run from WSL (vhs hangs on Set Theme in native Windows).
# Note: FontFamily must be installed in the system. Adjust if tofu appears.

Output browse.gif
Set Shell "bash"
Set FontSize 14
Set FontFamily "PlemolJP Console NF"
Set Width 1200
Set Height 720
Set Padding 20
Set TypingSpeed 50ms
Set Theme "Catppuccin Mocha"

# Clean slate and pre-seed a couple of plugins so the ✓ mark has something to show.
Hide
Type "rm -rf ~/.config/rvpm ~/.cache/rvpm 2>/dev/null"
Enter
Sleep 200ms
Type "rvpm init --write >/dev/null"
Enter
Sleep 2s
Type "rvpm add folke/snacks.nvim >/dev/null 2>&1"
Enter
Sleep 2s
Type "rvpm add nvim-telescope/telescope.nvim >/dev/null 2>&1"
Enter
Sleep 2s

# Pre-warm the GitHub search cache so the recorded launch is instant.
# `rvpm browse` awaits the initial fetch before it enters the main loop,
# so by the time we send `q` the 24h cache under
# ~/.cache/rvpm/<appname>/browse/ is already populated.
Type "rvpm browse"
Enter
Sleep 8s
Type "q"
Sleep 500ms
Type "clear"
Enter
Sleep 200ms
Show

# Launch the browse TUI — reads from cache, opens immediately.
Sleep 500ms
Type "rvpm browse"
Sleep 500ms
Enter
Sleep 1500ms

# Navigate the plugin list.
Type "j"
Sleep 400ms
Type "j"
Sleep 400ms
Type "j"
Sleep 400ms
Type "k"
Sleep 600ms

# Incremental local search.
Type "/"
Sleep 300ms
Type "tele"
Sleep 1500ms
Enter
Sleep 800ms

# Jump between matches with n / N.
Type "n"
Sleep 500ms
Type "n"
Sleep 500ms
Type "N"
Sleep 800ms

# Switch focus to the README pane and scroll.
Tab
Sleep 600ms
Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Ctrl+d
Sleep 800ms
Type "G"
Sleep 1s
Type "g"
Sleep 800ms

# Back to the list.
Tab
Sleep 500ms

# Toggle help popup.
Type "?"
Sleep 3s
Type "?"
Sleep 500ms

# Quit.
Type "q"
Sleep 1s
