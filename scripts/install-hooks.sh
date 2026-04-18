#!/usr/bin/env sh
set -eu

if command -v lefthook >/dev/null 2>&1; then
  lefthook install
elif [ -x "$HOME/.local/bin/lefthook" ]; then
  "$HOME/.local/bin/lefthook" install
else
  echo "lefthook is not installed; install it or set LEFTHOOK_BIN before committing" >&2
  exit 1
fi

