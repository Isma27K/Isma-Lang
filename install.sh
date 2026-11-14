#!/usr/bin/env bash
# Install Isma so `isma` works from any directory, like `python`.
#
#   ./install.sh               install to ~/.local/bin (no sudo needed)
#   PREFIX=/usr/local sudo -E ./install.sh   install system-wide
#   ./install.sh --uninstall   remove the binary and the compile cache
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
CACHE_DIR="${ISMA_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/isma}"
cd "$(dirname "$0")"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

if [[ "${1:-}" == "--uninstall" ]]; then
    rm -f "$BIN_DIR/isma"
    rm -rf "$CACHE_DIR"
    bold "Isma uninstalled (removed $BIN_DIR/isma and $CACHE_DIR)"
    exit 0
fi

command -v cargo > /dev/null || die "cargo not found. Install Rust first: https://rustup.rs"

# Isma programs are compiled to native code by clang, so it must be present at runtime.
if [[ -z "${ISMA_CC:-}" ]] && ! command -v clang > /dev/null && ! compgen -G "/usr/lib/llvm-*/bin/clang" > /dev/null; then
    warn "clang was not found. Isma needs it to compile programs. Install it with:"
    warn "  Debian/Ubuntu: sudo apt install clang"
    warn "  Fedora:        sudo dnf install clang"
    warn "  Arch:          sudo pacman -S clang"
    warn "  macOS:         xcode-select --install"
fi

bold "Building isma (release)..."
cargo build --release --quiet

mkdir -p "$BIN_DIR"
install -m 755 target/release/isma "$BIN_DIR/isma"
bold "Installed $("$BIN_DIR/isma" --version) to $BIN_DIR/isma"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        warn "$BIN_DIR is not on your PATH. Add this line to ~/.bashrc (or ~/.zshrc):"
        warn "  export PATH=\"$BIN_DIR:\$PATH\""
        ;;
esac

cat <<EOF

Try it:
  echo 'fn main():
      print("hello from anywhere")' > /tmp/hi.isma
  isma /tmp/hi.isma
EOF
