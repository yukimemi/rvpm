#!/usr/bin/env bash
# Regenerate vhs/demo.gif (and any other tape passed in $1) using the
# `rvpm-vhs` Docker image built from the sibling Dockerfile.
#
# Run from WSL bash. From PowerShell you can do:
#   wsl --cd /mnt/c/Users/<you>/src/.../rvpm/vhs -- bash regen.sh
#
# Requires:
#   - docker (whether real `docker` or our `wsl docker` PowerShell wrapper)
#   - rvpm-vhs image built: `docker build -t rvpm-vhs .`
#   - An AI API key matching the backend used by the tape (interactive
#     OAuth login flow doesn't survive a one-shot non-interactive
#     container, so an API key is the practical option):
#       Gemini : GEMINI_API_KEY    https://aistudio.google.com/apikey
#       Claude : ANTHROPIC_API_KEY https://console.anthropic.com/
#       Codex  : OPENAI_API_KEY    https://platform.openai.com/api-keys
#     The script forwards whichever of the three are set in the host
#     environment, so the same call works for any backend.

set -euo pipefail

TAPE="${1:-demo.tape}"

if [[ -z "${GEMINI_API_KEY:-}" && -z "${ANTHROPIC_API_KEY:-}" && -z "${OPENAI_API_KEY:-}" ]]; then
    echo "No AI API key found in environment." >&2
    echo "Set one of these to match your tape's --ai backend:" >&2
    echo "  GEMINI_API_KEY=...     (https://aistudio.google.com/apikey)" >&2
    echo "  ANTHROPIC_API_KEY=...  (https://console.anthropic.com/)" >&2
    echo "  OPENAI_API_KEY=...     (https://platform.openai.com/api-keys)" >&2
    exit 1
fi

cd "$(dirname "$0")"

exec docker run --rm \
    -v "$PWD:/vhs" \
    -e GEMINI_API_KEY="${GEMINI_API_KEY:-}" \
    -e ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}" \
    -e OPENAI_API_KEY="${OPENAI_API_KEY:-}" \
    rvpm-vhs "$TAPE"
