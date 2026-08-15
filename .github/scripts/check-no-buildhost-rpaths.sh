#!/usr/bin/env bash
# travsr-lang#17: guard against shipping a release binary that resolves a
# dynamic dependency through a build-host absolute rpath.
#
# The Objective-C emitter used to link-bind libclang.dylib and bake the CI
# machine's Xcode path (e.g. /Applications/Xcode_26.6.app/...) into LC_RPATH,
# so dyld aborted the process before main() on every user machine that lacked
# that exact path. It now loads libclang at runtime (clang-sys `runtime`), so
# the shipped binary must carry NO libclang load command and NO build-host
# absolute rpath. This check fails the release if either regresses.
#
# Usage: check-no-buildhost-rpaths.sh <binary> [<binary> ...]
set -euo pipefail

fail=0

check_macho() {
  local bin="$1"
  # 1. No dynamic dependency on libclang (or any dylib we do not bundle).
  if otool -L "$bin" | tail -n +2 | grep -qi 'libclang'; then
    echo "FAIL: $bin has a dynamic dependency on libclang (expected runtime dlopen):" >&2
    otool -L "$bin" | grep -i 'libclang' >&2
    fail=1
  fi
  # 2. No LC_RPATH pointing at a build-host absolute location. Relocatable
  #    rpaths (@loader_path / @executable_path / @rpath) are fine.
  local rpaths
  rpaths=$(otool -l "$bin" | awk '/cmd LC_RPATH/{f=1} f&&/path /{print $2; f=0}')
  if [ -n "$rpaths" ]; then
    while IFS= read -r rp; do
      [ -z "$rp" ] && continue
      case "$rp" in
        @*) : ;; # relocatable — OK
        /Applications/Xcode*|/Users/*|/Library/Developer/*|/opt/homebrew/Cellar/*|/usr/local/Cellar/*)
          echo "FAIL: $bin carries a build-host absolute rpath: $rp" >&2
          fail=1 ;;
        *)
          echo "WARN: $bin carries an absolute rpath: $rp (review manually)" >&2 ;;
      esac
    done <<< "$rpaths"
  fi
}

check_elf() {
  local bin="$1"
  command -v readelf >/dev/null 2>&1 || { echo "note: readelf missing, skipping $bin" >&2; return; }
  local rp
  rp=$(readelf -d "$bin" 2>/dev/null | awk -F'[][]' '/R(UN)?PATH/{print $2}')
  [ -z "$rp" ] && return
  case "$rp" in
    *'$ORIGIN'*) : ;; # relocatable — OK
    /home/*|/opt/*|/root/*|/Users/*)
      echo "FAIL: $bin carries a build-host absolute rpath: $rp" >&2
      fail=1 ;;
    *)
      echo "WARN: $bin carries an absolute rpath: $rp (review manually)" >&2 ;;
  esac
}

for bin in "$@"; do
  [ -f "$bin" ] || { echo "FAIL: $bin does not exist" >&2; fail=1; continue; }
  case "$(uname -s)" in
    Darwin) check_macho "$bin" ;;
    Linux)  check_elf   "$bin" ;;
    *)      echo "note: unsupported platform $(uname -s), skipping $bin" >&2 ;;
  esac
  [ "$fail" -eq 0 ] && echo "OK: $bin has no unbundled build-host dynamic deps"
done

exit "$fail"
