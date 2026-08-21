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
  local this_fail=0

  # 1. No dynamic dependency on libclang (or any dylib we do not bundle).
  #    Capture otool -L output once: under `set -o pipefail`, an early-exiting
  #    `grep -q` can SIGPIPE its upstream and make the pipeline report 141,
  #    which the `if` would misread as "no match" for the check that matters
  #    most here (review finding 7).
  local deps
  deps=$(otool -L "$bin" | tail -n +2)
  if printf '%s\n' "$deps" | grep -qi 'libclang'; then
    echo "FAIL: $bin has a dynamic dependency on libclang (expected runtime dlopen):" >&2
    printf '%s\n' "$deps" | grep -i 'libclang' >&2
    this_fail=1
  fi

  # 2. No LC_RPATH pointing at a build-host absolute location. Relocatable
  #    rpaths (@loader_path / @executable_path / @rpath) are fine. Match each
  #    rpath line individually so multiple LC_RPATH commands cannot be routed
  #    to WARN by matching against a combined string.
  local rpaths
  rpaths=$(otool -l "$bin" | awk '/cmd LC_RPATH/{f=1} f&&/path /{print $2; f=0}')
  while IFS= read -r rp; do
    [ -z "$rp" ] && continue
    case "$rp" in
      @*) : ;; # relocatable, OK
      /Applications/Xcode*|/Users/*|/Library/Developer/*|/opt/homebrew/Cellar/*|/usr/local/Cellar/*)
        echo "FAIL: $bin carries a build-host absolute rpath: $rp" >&2
        this_fail=1 ;;
      *)
        echo "WARN: $bin carries an absolute rpath: $rp (review manually)" >&2 ;;
    esac
  done <<< "$rpaths"

  if [ "$this_fail" -eq 0 ]; then
    echo "OK: $bin has no unbundled build-host dynamic deps"
  else
    fail=1
  fi
}

check_elf() {
  local bin="$1"
  local this_fail=0
  command -v readelf >/dev/null 2>&1 || { echo "note: readelf missing, skipping $bin" >&2; return; }

  # 1. No DT_NEEDED dependency on libclang, the Linux analogue of the otool -L
  #    check, so the script's headline claim ("fails on a link-time libclang
  #    dependency") holds on Linux too, not only Darwin (review finding 6).
  local needed
  needed=$(readelf -d "$bin" 2>/dev/null | awk -F'[][]' '/NEEDED/{print $2}')
  if printf '%s\n' "$needed" | grep -qi 'libclang'; then
    echo "FAIL: $bin has a DT_NEEDED dependency on libclang (expected runtime dlopen):" >&2
    printf '%s\n' "$needed" | grep -i 'libclang' >&2
    this_fail=1
  fi

  # 2. No RPATH/RUNPATH pointing at a build-host absolute location. readelf can
  #    emit both an RPATH and a RUNPATH tag; match each line individually rather
  #    than the combined two-line string (review finding 6).
  local rpaths
  rpaths=$(readelf -d "$bin" 2>/dev/null | awk -F'[][]' '/R(UN)?PATH/{print $2}')
  while IFS= read -r rp; do
    [ -z "$rp" ] && continue
    case "$rp" in
      *'$ORIGIN'*) : ;; # relocatable, OK
      /home/*|/opt/*|/root/*|/Users/*)
        echo "FAIL: $bin carries a build-host absolute rpath: $rp" >&2
        this_fail=1 ;;
      *)
        echo "WARN: $bin carries an absolute rpath: $rp (review manually)" >&2 ;;
    esac
  done <<< "$rpaths"

  if [ "$this_fail" -eq 0 ]; then
    echo "OK: $bin has no unbundled build-host dynamic deps"
  else
    fail=1
  fi
}

for bin in "$@"; do
  [ -f "$bin" ] || { echo "FAIL: $bin does not exist" >&2; fail=1; continue; }
  case "$(uname -s)" in
    Darwin) check_macho "$bin" ;;
    Linux)  check_elf   "$bin" ;;
    *)      echo "note: unsupported platform $(uname -s), skipping $bin" >&2 ;;
  esac
done

exit "$fail"
