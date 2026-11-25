#!/usr/bin/env bash
set -euo pipefail

export PATH="/usr/lib/llvm-18/bin:$PATH"
PROFILE="${1:-debug}"

if [[ "$PROFILE" == "release" ]]; then
  cargo xwin build --release --target x86_64-pc-windows-msvc --bin same
  EXE="target/x86_64-pc-windows-msvc/release/same.exe"
else
  cargo xwin build --target x86_64-pc-windows-msvc --bin same
  EXE="target/x86_64-pc-windows-msvc/debug/same.exe"
fi

WIN_EXE="$(wslpath -w "$EXE")"
echo "Built: $EXE"
echo "Windows path: $WIN_EXE"
