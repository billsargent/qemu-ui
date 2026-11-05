#!/usr/bin/env bash
# Install every binary the project needs into ./.tools (gitignored).
# Nothing is installed system-wide and PATH is not modified permanently.
#
#   scripts/bootstrap.sh            # install everything missing
#   scripts/bootstrap.sh rust       # only one component: rust | cc | qemu | node-deps
#   source scripts/env.sh           # put the project tools on PATH for this shell
#
# Works in Git Bash on Windows and in bash on Linux.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLS="$ROOT/.tools"
DL="$TOOLS/downloads"
mkdir -p "$DL"

# Pinned versions. Bump deliberately, in a commit.
RUST_TOOLCHAIN="stable"
W64DEVKIT_VERSION="2.11.0"
QEMU_WIN_BUILD="20260811"          # https://qemu.weilnetz.de/w64/

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) OS=windows ;;
  Linux*)               OS=linux ;;
  Darwin*)              OS=macos ;;
  *) echo "unsupported host: $(uname -s)" >&2; exit 1 ;;
esac

log() { printf '\033[1;34m[bootstrap]\033[0m %s\n' "$*"; }

fetch() { # url dest
  if [[ -s "$2" ]]; then log "cached $(basename "$2")"; return; fi
  log "downloading $1"
  curl -fL --retry 3 -o "$2.part" "$1"
  mv "$2.part" "$2"
}

install_rust() {
  export RUSTUP_HOME="$TOOLS/rustup" CARGO_HOME="$TOOLS/cargo"
  if [[ -x "$CARGO_HOME/bin/cargo" || -x "$CARGO_HOME/bin/cargo.exe" ]]; then
    log "rust already installed"; return
  fi
  local host init
  if [[ $OS == windows ]]; then
    # GNU host: self-contained linker, no Visual Studio required.
    host="x86_64-pc-windows-gnu"; init="$DL/rustup-init.exe"
    fetch "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe" "$init"
  else
    host="$(uname -m)-unknown-linux-gnu"; [[ $OS == macos ]] && host="$(uname -m | sed s/arm64/aarch64/)-apple-darwin"
    init="$DL/rustup-init"
    fetch "https://static.rust-lang.org/rustup/dist/$host/rustup-init" "$init"
    chmod +x "$init"
  fi
  "$init" -y --no-modify-path --profile minimal \
    --default-host "$host" --default-toolchain "$RUST_TOOLCHAIN" -c clippy -c rustfmt
}

install_cc() {
  # A C compiler is needed by crates that build C (bundled SQLite, ring).
  [[ $OS != windows ]] && { log "cc: using system compiler on $OS"; return; }
  if [[ -x "$TOOLS/w64devkit/bin/gcc.exe" ]]; then log "w64devkit already installed"; fix_libgcc_eh; return; fi
  local f="$DL/w64devkit-x64-$W64DEVKIT_VERSION.7z.exe"
  fetch "https://github.com/skeeto/w64devkit/releases/download/v$W64DEVKIT_VERSION/w64devkit-x64-$W64DEVKIT_VERSION.7z.exe" "$f"
  local sz; sz="$(command -v 7z || echo "/c/Program Files/7-Zip/7z")"
  if [[ -x "$sz" || -x "$sz.exe" ]]; then
    "$sz" x -y -o"$TOOLS" "$f" >/dev/null
  else
    # The archive is a self-extracting 7z.
    (cd "$TOOLS" && "$f" -y -o"$TOOLS" >/dev/null)
  fi
  fix_libgcc_eh
}

fix_libgcc_eh() {
  # Rust's windows-gnu target links with -lgcc_eh. w64devkit builds GCC without a
  # shared libgcc, so the unwinder lives in libgcc.a and libgcc_eh.a is absent.
  # An empty archive satisfies the linker without changing what gets linked.
  local libgcc; libgcc="$("$TOOLS/w64devkit/bin/gcc.exe" -print-libgcc-file-name)"
  local eh; eh="$(dirname "$libgcc")/libgcc_eh.a"
  if [[ ! -f "$eh" ]]; then
    "$TOOLS/w64devkit/bin/ar.exe" rc "$eh"
    log "created empty libgcc_eh.a for the Rust GNU target"
  fi
}

install_qemu() {
  if [[ $OS != windows ]]; then
    log "qemu: on $OS use the distro package (e.g. apt install qemu-system qemu-utils);"
    log "      the app probes PATH and .tools/qemu, so either location works."
    return
  fi
  if [[ -x "$TOOLS/qemu/qemu-img.exe" ]]; then log "qemu already installed"; return; fi
  local f="$DL/qemu-w64-setup-$QEMU_WIN_BUILD.exe"
  fetch "https://qemu.weilnetz.de/w64/qemu-w64-setup-$QEMU_WIN_BUILD.exe" "$f"
  # NSIS installer: extract with 7-Zip rather than running it, so nothing touches the registry.
  local sz; sz="$(command -v 7z || echo "/c/Program Files/7-Zip/7z")"
  rm -rf "$TOOLS/qemu"; mkdir -p "$TOOLS/qemu"
  "$sz" x -y -o"$TOOLS/qemu" "$f" >/dev/null
  rm -rf "$TOOLS/qemu/\$PLUGINSDIR" "$TOOLS/qemu/uninstall.exe"
  "$TOOLS/qemu/qemu-img.exe" --version | head -1
}

install_node_deps() {
  command -v npm >/dev/null || { log "npm not found; install Node 20+ to build the web UI"; return; }
  (cd "$ROOT/web" && npm ci --no-audit --no-fund 2>/dev/null || npm install --no-audit --no-fund)
}

what="${1:-all}"
case "$what" in
  rust) install_rust ;;
  cc) install_cc ;;
  qemu) install_qemu ;;
  node-deps) install_node_deps ;;
  all) install_cc; install_rust; install_qemu; [[ -d "$ROOT/web" ]] && install_node_deps ;;
  *) echo "unknown component: $what" >&2; exit 2 ;;
esac
log "done. Run: source scripts/env.sh"
