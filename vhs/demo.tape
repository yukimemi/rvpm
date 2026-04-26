# rvpm demo — README GIF
# Usage:
#   Native (Linux/macOS):  cd vhs && vhs demo.tape
#   Docker (recommended on Windows — native vhs hangs on Set Theme):
#     cd vhs && docker build -t rvpm-vhs . && \
#       docker run --rm -v "$PWD:/vhs" -e ANTHROPIC_API_KEY rvpm-vhs demo.tape
#     (or just `bash regen.sh` from this directory once the image is built).
# Note: FontFamily must be installed in the system. Adjust if tofu appears.
# Note: The tape runs in an isolated NVIM_APPNAME/RVPM_APPNAME sandbox
#       (`rvpm-demo`) and only ever touches paths under that appname. It
#       will NOT modify your real `~/.config/nvim/` or `~/.config/rvpm/`.
#
# Flow (single gif covering both list and browse TUIs):
#   1. init --write
#   2. rvpm add dstein64/vim-startuptime --ai gemini   (AI-assisted add)
#      - gemini CLI must be on PATH and authenticated for this to work
#      - vim-startuptime is intentionally tiny (single :StartupTime
#        command, no Lua setup) so the AI proposal preview fits in
#        the GIF window without truncation
#      - the tape presses Enter to Apply the AI proposal as-is
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
Set FontSize 16
Set FontFamily "PlemolJP Console NF"
Set Width 900
Set Height 600
Set Padding 20
Set TypingSpeed 50ms
Set Theme "Catppuccin Mocha"

# ── clean slate ──
# NVIM_APPNAME / RVPM_APPNAME で完全に sandbox 化し、ユーザーの実 nvim /
# rvpm 設定 (`~/.config/nvim/`, `~/.config/rvpm/nvim/`) には触れないようにする。
Hide
Type "export NVIM_APPNAME=rvpm-demo RVPM_APPNAME=rvpm-demo"
Enter
Sleep 200ms
Type "rm -rf ~/.config/rvpm/rvpm-demo ~/.cache/rvpm/rvpm-demo ~/.config/rvpm-demo 2>/dev/null"
Enter
Sleep 500ms
Type "clear"
Enter
Sleep 200ms
Show

# ── 1. init ──
Sleep 500ms
Type "rvpm init --write"
Sleep 500ms
Enter
Sleep 2500ms

# ── 2. AI-assisted CLI add ──
# `rvpm add ... --ai gemini` runs the chat loop: AI proposal preview →
# "How should we proceed?" Select menu (default = Apply) → for each hook
# file the AI proposes, a per-file dialog (default = Use FRESH).
Type "rvpm add dstein64/vim-startuptime --ai gemini"
Sleep 500ms
Enter
# Show the "🤖 Asking Gemini..." status line briefly so viewers know
# what's happening, then Hide while the actual API round-trip runs —
# vhs keeps elapsing real time but the GIF skips those frames, so we
# don't bake the AI wait into the output. Show resumes recording when
# the preview is expected on screen.
Sleep 1500ms
Hide
Sleep 38500ms
Show
# Preview is on screen; let it linger briefly before pressing Enter.
Sleep 2s
# Accept "Apply" on the action menu (it is the default).
Enter
Sleep 3s
# Note: vim-startuptime needs no Lua hooks, so the AI proposes
# `(none)` for init/before/after.lua and the per-file dialogs never
# fire. If this tape is repointed at a plugin that does need hooks,
# add an `Enter` + `Sleep 800ms` here for each hook the AI proposes
# so the default "Use FRESH" gets accepted.

# ── 3. silent seed: append raw entries so `list` has rows and the later
#    `S` sync fires actual clones. The seeded plugins are mainstream
#    neovim-plugin-topic entries so browse's `✓` marker lights up for
#    them when we visit the browse TUI in step 5. ──
Hide
Type `printf '\n[[plugins]]\nurl = "%s"\n' nvim-lua/plenary.nvim stevearc/oil.nvim MunifTanjim/nui.nvim lewis6991/gitsigns.nvim >> ~/.config/rvpm/$RVPM_APPNAME/config.toml`
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
Sleep 400ms
Type "G"
Sleep 600ms

# search — find the plugin we just AI-added
Type "/"
Sleep 400ms
Type "startup"
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
