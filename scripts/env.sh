# Source me: puts the project-local toolchain and QEMU first on PATH.
#   source scripts/env.sh
_QUI_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
export RUSTUP_HOME="$_QUI_ROOT/.tools/rustup"
export CARGO_HOME="$_QUI_ROOT/.tools/cargo"
export PATH="$CARGO_HOME/bin:$_QUI_ROOT/.tools/w64devkit/bin:$_QUI_ROOT/.tools/qemu:$PATH"
export QEMU_UI_QEMU_DIR="$_QUI_ROOT/.tools/qemu"
unset _QUI_ROOT
