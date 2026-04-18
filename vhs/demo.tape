# rvpm demo — README GIF
# Usage: (cd vhs && vhs demo.tape)   # output lands next to this tape
# Note: On Windows, run from WSL (vhs hangs on Set Theme in native Windows).
# Note: FontFamily must be installed in the system. Adjust if tofu appears.
#
# Flow (single gif covering both list and browse TUIs):
#   1. init --write
#   2. rvpm add folke/snacks.nvim                  (CLI add)
#   3. silently seed extra plugins for a lively sync UI later
#   4. rvpm list                                   (list TUI tour)
#      - j/k navigation, / search
#      - b → browse TUI
#   5. browse TUI tour
#      - j/k navigation, / incremental search, Tab → README pane, scroll
#      - Enter to add the highlighted plugin (browse's one-shot add)
#      - l → back to list TUI
#   6. S to trigger an animated sync
#   7. q

Output demo.gif
Set Shell "bash"
Set FontSize 15
Set FontFamily "PlemolJP Console NF"
Set Width 1200
Set Height 720
Set Padding 20
Set TypingSpeed 50ms
Set Theme "Catppuccin Mocha"

# ── clean slate ──
Hide
Type "rm -rf ~/.config/rvpm ~/.cache/rvpm ~/.config/nvim/init.lua 2>/dev/null; clear"
Enter
Sleep 500ms
Show

# ── 1. init ──
Sleep 500ms
Type "rvpm init --write"
Sleep 500ms
Enter
Sleep 2500ms

# ── 2. CLI add ──
Type "rvpm add folke/snacks.nvim"
Sleep 500ms
Enter
Sleep 2500ms

# ── 3. silent seed: append raw entries so `list` has rows and the later
#    `S` sync fires actual clones. Plus, `browse`'s `✓` marker works
#    because snacks.nvim is already in config. ──
Hide
Type `printf '\n[[plugins]]\nurl = "%s"\n' nvim-lua/plenary.nvim stevearc/oil.nvim MunifTanjim/nui.nvim lewis6991/gitsigns.nvim >> ~/.config/rvpm/nvim/config.toml`
Enter
Sleep 300ms
# Pre-warm the GitHub search cache so the later `rvpm list → b` jump into
# browse doesn't sit on a blank screen waiting for the API round-trip.
Type "rvpm browse"
Enter
Sleep 7s
Type "q"
Sleep 500ms
Type "clear"
Enter
Sleep 200ms
Show

# ── 4. list TUI tour ──
Sleep 500ms
Type "rvpm list"
Sleep 500ms
Enter
Sleep 2s

# navigate
Type "j"
Sleep 400ms
Type "j"
Sleep 400ms
Type "k"
Sleep 600ms

# search
Type "/"
Sleep 400ms
Type "snacks"
Sleep 1200ms
Enter
Sleep 800ms

# ── 5. jump into browse TUI ──
Type "b"
Sleep 2s

# browse navigation
Type "j"
Sleep 400ms
Type "j"
Sleep 400ms
Type "j"
Sleep 400ms
Type "k"
Sleep 500ms

# browse incremental search
Type "/"
Sleep 300ms
Type "tele"
Sleep 1500ms
Enter
Sleep 600ms

# Tab to README pane, scroll a bit
Tab
Sleep 400ms
Type "j"
Sleep 250ms
Type "j"
Sleep 250ms
Ctrl+d
Sleep 800ms
Type "G"
Sleep 600ms
Type "g"
Sleep 500ms
Tab
Sleep 400ms

# Add highlighted plugin from browse TUI
Enter
Sleep 5s
# "Press any key to return to browse..."
Space
Sleep 1500ms

# ── 6. back to list, then full sync ──
Type "l"
Sleep 2s

Type "S"
Sleep 10s
# "Press any key to return to list..." prompt
Enter
Sleep 1500ms

# ── 7. quit ──
Type "q"
Sleep 1s
