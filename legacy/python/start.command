#!/bin/zsh
set -eu
cd "${0:A:h}"
if [[ ! -x .venv/bin/python ]]; then
  python3 -m venv .venv
  .venv/bin/python -m pip install -r requirements.txt
fi
if [[ -f "$HOME/Library/LaunchAgents/local.codex-wiretap.plist" ]]; then
  exec launchctl kickstart "gui/$(id -u)/local.codex-wiretap"
fi
exec .venv/bin/python wiretap.py serve
