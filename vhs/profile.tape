# rvpm profile demo — README GIF
# Usage: (cd vhs && vhs profile.tape)   # output lands next to this tape
# Note: On Windows, run from WSL (vhs hangs on Set Theme in native Windows).
# Note: FontFamily must be installed in the system. Adjust if tofu appears.
# Note: The tape runs in an isolated NVIM_APPNAME/RVPM_APPNAME sandbox
#       (`rvpm-profile-demo`) and only ever touches paths under that appname.
#       It will NOT modify your real `~/.config/nvim/` or `~/.config/rvpm/`.
#
# Flow:
#   1. init --write
#   2. seed a handful of plugins and sync (so profile has something to measure)
#   3. rvpm profile --runs 2         (instrumented, TUI opens automatically)
#   4. TUI tour: j/k navigate, s cycle sort, h hide groups, ? help, q
#
# The profile TUI needs enough rows for banner(3) + timeline(9) + table(min 6)
# + detail(9) + footer(3) — bump Height to 900 (the demo.tape uses 600).

Output profile.gif
Set Shell "bash"
Set FontSize 16
Set FontFamily "PlemolJP Console NF"
Set Width 1000
Set Height 900
Set Padding 20
Set TypingSpeed 50ms
Set Theme "Catppuccin Mocha"

# ── clean slate ──
# NVIM_APPNAME / RVPM_APPNAME で完全に sandbox 化し、ユーザーの実 nvim /
# rvpm 設定には触れないようにする。demo.tape とは別 appname を使うことで
# 両 tape を並行して撮れる。
Hide
Type "export NVIM_APPNAME=rvpm-profile-demo RVPM_APPNAME=rvpm-profile-demo"
Enter
Sleep 200ms
Type "rm -rf ~/.config/rvpm/rvpm-profile-demo ~/.cache/rvpm/rvpm-profile-demo ~/.config/rvpm-profile-demo 2>/dev/null; clear"
Enter
Sleep 500ms
Show

# ── 1. init ──
Sleep 500ms
Type "rvpm init --write"
Sleep 500ms
Enter
Sleep 2s

# ── 2. seed a realistic plugin set, then sync ──
# mixing eager + lazy + a couple of bigger plugins gives the profile timeline
# and distribution bars something interesting to show.
Hide
Type `cat >> ~/.config/rvpm/$RVPM_APPNAME/config.toml <<'TOML'

[[plugins]]
url = "folke/snacks.nvim"

[[plugins]]
url = "nvim-lua/plenary.nvim"

[[plugins]]
url = "stevearc/oil.nvim"

[[plugins]]
url = "lewis6991/gitsigns.nvim"

[[plugins]]
url = "nvim-treesitter/nvim-treesitter"

[[plugins]]
url = "nvim-telescope/telescope.nvim"
on_cmd = ["Telescope"]
TOML`
Enter
Sleep 300ms
Type "clear"
Enter
Sleep 200ms
Show

Type "rvpm sync"
Sleep 500ms
Enter
# sync needs time to clone 6 plugins in parallel + build helptags
Sleep 12s
Type "clear"
Enter
Sleep 500ms

# ── 3. run profile (instrumented by default, opens TUI) ──
# --runs 2 keeps it under ~6 seconds per measurement round while still giving
# a stable average; the default is 3.
Type "rvpm profile --runs 2"
Sleep 500ms
Enter
# nvim --headless --startuptime runs 2x (instrumented) — each run ~2-3s on
# a sandbox clone. Wait long enough for the TUI to open.
Sleep 8s

# ── 4. TUI tour ──
# Let the viewer absorb the initial screen (banner + phase timeline + table +
# detail) for a beat before anything moves.
Sleep 1500ms

# navigate — each step re-renders the detail panel for the newly selected plugin
Type "j"
Sleep 700ms
Type "j"
Sleep 700ms
Type "j"
Sleep 700ms
Type "k"
Sleep 700ms
Type "G"
Sleep 1s
Type "g"
Sleep 1s

# cycle sort: load → init → trig → total → files → name (→ load)
# each press swaps the active column header style + re-sorts the table.
Type "s"
Sleep 900ms
Type "s"
Sleep 900ms
Type "s"
Sleep 900ms
Type "s"
Sleep 900ms

# hide pseudo-groups ([merged] / [runtime] etc.)
Type "h"
Sleep 1200ms
Type "h"
Sleep 800ms

# help overlay
Type "?"
Sleep 1800ms
Type "?"
Sleep 700ms

# ── 5. quit ──
Type "q"
Sleep 800ms
