#!/usr/bin/env bash
# Package built wrapper binaries into dist/ as release assets, with a SHA256
# sidecar for each.
#
# Usage: package-wrappers.sh <target-triple> [profile-dir]
#          target-triple  e.g. x86_64-pc-windows-msvc
#          profile-dir    cargo profile directory, default "release"
#
# Extracted from the release workflow so the Windows CI job can run the exact
# same packaging on every PR. Before this, the naming rule below was only ever
# exercised during a tag: a mistake in it surfaced after the release was
# already half-published, which is the failure mode travsr#588 is about.
#
# The asset name produced here is a contract with travsr's installer
# (crates/travsr-cli/src/install.rs, `wrapper_asset_name`). If the two disagree
# by even an extension, `travsr lang install <lang>` 404s.
set -euo pipefail

TARGET="${1:?usage: package-wrappers.sh <target-triple> [profile-dir]}"
PROFILE="${2:-release}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINS_FILE="${SCRIPT_DIR}/../wrapper-bins.txt"

# Windows executables carry `.exe`, and the sidecar is named after the full
# asset (`<bin>-<target>.exe.sha256`), not after the extensionless stem.
case "$TARGET" in
  *windows*) EXE=".exe" ;;
  *)         EXE=""     ;;
esac

# Portable SHA256: sha256sum on Linux / Git Bash, shasum on macOS.
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  echo "ERROR: neither sha256sum nor shasum found on PATH" >&2
  exit 1
fi

mkdir -p dist

# Comments and blank lines stripped; the rest are binary names.
BINS=$(grep -vE '^[[:space:]]*(#|$)' "$BINS_FILE")

count=0
for bin in $BINS; do
  src="target/${TARGET}/${PROFILE}/${bin}${EXE}"
  if [ ! -f "$src" ]; then
    echo "ERROR: expected built binary not found: $src" >&2
    exit 1
  fi

  dst="dist/${bin}-${TARGET}${EXE}"
  cp "$src" "$dst"

  # strip reduces binary size; skipped silently on cross-compiled targets where
  # the host strip does not understand the target arch. Never run on MSVC
  # output: the MSYS strip in Git Bash mangles PE/COFF produced by link.exe
  # rather than failing cleanly, so the guard is an explicit skip, not a
  # swallowed error.
  if [ -z "$EXE" ]; then
    strip "$dst" 2>/dev/null || true
  fi

  sha256 "$dst" > "${dst}.sha256"
  count=$((count + 1))
done

echo "packaged ${count} wrapper(s) for ${TARGET}:"
ls -1 dist/
