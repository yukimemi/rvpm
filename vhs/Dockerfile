# Image for regenerating vhs/demo.gif on machines where native vhs is
# unreliable (Windows in particular — vhs hangs on `Set Theme` there).
#
# Build (from this directory):
#   docker build -t rvpm-vhs .
#
# Run (regenerate demo.gif into the current dir):
#   docker run --rm \
#     -v "$PWD:/vhs" \
#     -e GEMINI_API_KEY="$GEMINI_API_KEY" \
#     rvpm-vhs demo.tape
#
# An AI API key matching whichever backend the tape uses (`--ai gemini`
# / `--ai claude` / `--ai codex`) is required because each CLI's
# interactive OAuth login flow can't survive a one-shot container.
# Generate a key at:
#   - Gemini : https://aistudio.google.com/apikey   (env GEMINI_API_KEY)
#   - Claude : https://console.anthropic.com/        (env ANTHROPIC_API_KEY)
#   - Codex  : https://platform.openai.com/api-keys  (env OPENAI_API_KEY)

FROM ghcr.io/charmbracelet/vhs:latest

# Toolchain for rvpm (Rust) + claude (Node) + git for plugin clones.
# build-essential / pkg-config / libssl-dev are needed by some of rvpm's
# transitive Rust deps (gix and friends).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        nodejs \
        npm \
        build-essential \
        pkg-config \
        libssl-dev \
        neovim \
        unzip \
        fontconfig \
    && rm -rf /var/lib/apt/lists/*

# PlemolJP Console NF — the font demo.tape uses (Set FontFamily ...).
# Without this, vhs renders the GIF with generic glyphs and Japanese
# / Nerd-Font icons turn into tofu. Pinned to a known release; bump
# the ARG when upstream tags a newer version.
ARG PLEMOLJP_VERSION=v3.0.0
RUN curl -fsSL -o /tmp/plemoljp_nf.zip \
        "https://github.com/yuru7/PlemolJP/releases/download/${PLEMOLJP_VERSION}/PlemolJP_NF_${PLEMOLJP_VERSION}.zip" \
    && mkdir -p /usr/share/fonts/truetype/plemoljp \
    && unzip -j /tmp/plemoljp_nf.zip "*.ttf" -d /usr/share/fonts/truetype/plemoljp \
    && fc-cache -f \
    && rm -f /tmp/plemoljp_nf.zip

# Rust (minimal profile — no docs, no clippy, etc. — just rustc + cargo).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# rvpm — pinned to the version this Dockerfile was tested against; bump
# alongside any release tag here so old images don't silently lag.
ARG RVPM_VERSION=3.30.3
RUN cargo install rvpm --version "${RVPM_VERSION}" --locked

# AI CLIs — install all three so the tape can switch backends without
# rebuilding. Layered separately so changing one doesn't bust the others'
# cache.
RUN npm install -g @anthropic-ai/claude-code
RUN npm install -g @google/gemini-cli
RUN npm install -g @openai/codex

# Pre-trust /vhs and /root for gemini-cli (≥ 0.39 enforces a
# "Trust this folder?" prompt on first run; in a one-shot container
# that prompt never gets answered and the AI call hangs/fails).
#   - /vhs    : bind-mount target the tape runs in
#   - /root   : TRUST_PARENT so rvpm's cache (~/.cache/rvpm/...) and
#               any other paths under HOME inherit trust automatically
RUN mkdir -p /root/.gemini && \
    printf '%s\n' '{' \
        '  "/vhs": "TRUST_FOLDER",' \
        '  "/root": "TRUST_PARENT"' \
        '}' > /root/.gemini/trustedFolders.json

# Default working dir; bind-mount the host vhs/ here at run time.
WORKDIR /vhs
