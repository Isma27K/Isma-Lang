#!/usr/bin/env bash
#  tarn installer
#
#    ./install.sh             interactive: default or custom install
#    ./install.sh -y          default install, no questions asked
#    ./install.sh --dir DIR   install into DIR, no questions asked
#
#  Flags: --no-path (never edit shell config)  --no-test (skip the self-test)
#         --no-anim (no spinner)               --uninstall (run ./uninstall.sh)
#  Env:   NO_COLOR=1 disables colors, TARN_NO_ANIM=1 disables the spinner.

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/tarn"
MANIFEST="$DATA_DIR/manifest"
DEFAULT_DIR="$HOME/.local/bin"

MODE=""        # default | custom | cli
BIN_DIR=""
EDIT_PATH=1
SELF_TEST=1
ANIM=1

usage() { sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^#//'; }

while [ $# -gt 0 ]; do
    case "$1" in
        -y | --yes | --default) MODE=default ;;
        --dir)
            [ $# -ge 2 ] || { echo "--dir needs a path" >&2; exit 2; }
            BIN_DIR="$2"; MODE=cli; shift ;;
        --no-path) EDIT_PATH=0 ;;
        --no-test) SELF_TEST=0 ;;
        --no-anim) ANIM=0 ;;
        --uninstall) shift; exec bash "$REPO/uninstall.sh" "$@" ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

INTERACTIVE=0
[ -t 0 ] && [ -t 1 ] && INTERACTIVE=1
TTY=0
[ -t 1 ] && TTY=1
[ "$TTY" = 1 ] || ANIM=0
[ -n "${TARN_NO_ANIM:-}" ] && ANIM=0

# ───────────────────────────────────────────────────────────── look
# Plain text, aligned columns, color only where it means something.
if [ "$TTY" = 1 ] && [ -z "${NO_COLOR:-}" ]; then
    B=$'\033[1m' D=$'\033[2m' OK=$'\033[32m' WARN=$'\033[33m' ERR=$'\033[31m' X=$'\033[0m'
else
    B="" D="" OK="" WARN="" ERR="" X=""
fi

cursor() { [ "$TTY" = 1 ] && printf '\033[?25%s' "$1"; return 0; }   # h = show, l = hide

BG_PID="" TMPD=""
cleanup() {
    [ -n "$BG_PID" ] && kill "$BG_PID" 2>/dev/null
    [ -n "$TMPD" ] && rm -rf "$TMPD"
    cursor h
}
trap cleanup EXIT
trap 'printf "\n\n  Cancelled. Nothing else was changed.\n\n"; exit 130' INT

# A row: a mark, a label and a value.  `step` shows the label while the work
# runs; `ok`, `warn` and `bad` finish the row.
LABEL=""
step() {
    LABEL="$1"
    [ "$TTY" = 1 ] && printf '    %s·%s  %-12s' "$D" "$X" "$LABEL"
    return 0
}
row() {
    [ "$TTY" = 1 ] && printf '\r\033[K'
    printf '    %s%s%s  %-12s%s\n' "$1" "$2" "$X" "$LABEL" "$3"
}
ok()   { row "$OK" "✓" "$1"; }
warn() { row "$WARN" "!" "$1"; }
bad()  { row "$ERR" "✗" "$1"; }
note() { printf '                   %s%s%s\n' "$D" "$1" "$X"; }
die()  { printf '\n  %s%s%s\n\n' "$ERR" "$1" "$X"; exit 1; }
tilde() { case "$1" in "$HOME"/*) echo "~${1#"$HOME"}" ;; *) echo "$1" ;; esac; }
section() { printf '\n  %s%s%s\n' "$B" "$1" "$X"; }

# Run a command in the background, with a spinner and its latest output line.
spin() {
    local log="$1" i=0 start=$SECONDS last
    shift
    "$@" > "$log" 2>&1 &
    BG_PID=$!
    if [ "$ANIM" = 1 ]; then
        local f=(⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏)
        cursor l
        while kill -0 "$BG_PID" 2> /dev/null; do
            last=$(tail -n 1 "$log" 2> /dev/null | tr -d '\r' | sed 's/^ *//' | cut -c1-40)
            printf '\r\033[K    %s%s%s  %-12s%s%ds  %s%s' "$D" "${f[$((i % 10))]}" "$X" "$LABEL" "$D" $((SECONDS - start)) "$last" "$X"
            i=$((i + 1))
            sleep 0.08
        done
        cursor h
    fi
    wait "$BG_PID"
    local status=$?
    BG_PID=""
    return $status
}

ask() {
    local var="$1" q="$2" def="$3" ans
    printf '  %s %s›%s ' "$q" "$D" "$X"
    read -r ans || ans=""
    [ -z "$ans" ] && ans="$def"
    printf -v "$var" '%s' "$ans"
}
yes_no() {
    local a hint="y/N"
    [ "$2" = y ] && hint="Y/n"
    ask a "$1 ${D}[$hint]${X}" "$2"
    case "$a" in y | Y | yes | YES) return 0 ;; *) return 1 ;; esac
}
option() { printf '    %s%s%s  %-14s %s%s%s\n' "$B" "$1" "$X" "$2" "$D" "$3" "$X"; }

find_clang() {
    if [ -n "${TARN_CC:-}" ]; then echo "$TARN_CC"; return; fi
    if command -v clang > /dev/null 2>&1; then command -v clang; return; fi
    local c
    for c in $(ls -d /usr/lib/llvm-*/bin/clang 2> /dev/null | sort -V -r); do
        echo "$c"; return
    done
}

shell_rc() {
    case "$(basename "${SHELL:-sh}")" in
        zsh) echo "${ZDOTDIR:-$HOME}/.zshrc" ;;
        bash) if [ "$(uname -s)" = Darwin ]; then echo "$HOME/.bash_profile"; else echo "$HOME/.bashrc"; fi ;;
        fish) echo "${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/tarn.fish" ;;
        *) echo "$HOME/.profile" ;;
    esac
}

remove_path_block() {
    local rc="$1" tmp
    [ -f "$rc" ] || return 0
    tmp=$(mktemp)
    awk '/^# >>> tarn >>>$/{skip=1} !skip{print} /^# <<< tarn <<<$/{skip=0}' "$rc" > "$tmp" && cat "$tmp" > "$rc"
    rm -f "$tmp"
}

add_path_block() {
    local rc="$1" dir="$2"
    mkdir -p "$(dirname "$rc")"
    case "$rc" in
        *.fish) printf '# added by the tarn installer\nfish_add_path "%s"\n' "$dir" > "$rc" ;;
        *)
            remove_path_block "$rc"
            if [ -s "$rc" ] && [ -n "$(tail -c 1 "$rc")" ]; then echo >> "$rc"; fi
            printf '# >>> tarn >>>\nexport PATH="%s:$PATH"\n# <<< tarn <<<\n' "$dir" >> "$rc"
            ;;
    esac
}

# ───────────────────────────────────────────────────────────── check
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -n 1)
TMPD=$(mktemp -d)

printf '\n  %starn%s %s· installer · v%s%s\n' "$B" "$X" "$D" "$VERSION" "$X"
printf '  %sPython'"'"'s syntax, Kotlin'"'"'s strictness, native speed%s\n' "$D" "$X"

section "Checking"

step "system"
ok "$(uname -s) $(uname -m), $(basename "${SHELL:-sh}")"

step "rust"
if command -v cargo > /dev/null 2>&1; then
    ok "$(cargo --version | cut -d' ' -f1-2)"
else
    bad "not found"
    die "Tarn is built with cargo. Install Rust first: https://rustup.rs"
fi

step "clang"
CLANG=$(find_clang)
if [ -n "$CLANG" ]; then
    ok "$("$CLANG" --version 2> /dev/null | head -n 1 | sed 's/.*clang version \([0-9.]*\).*/\1/')"
    note "$(tilde "$CLANG")"
else
    warn "not found"
    note "Tarn needs clang to turn your code into native programs:"
    note "apt install clang · dnf install clang · pacman -S clang · xcode-select --install"
fi

step "existing"
EXISTING=$(command -v tarn 2> /dev/null)
if [ -n "$EXISTING" ]; then
    ok "$("$EXISTING" --version 2> /dev/null || echo tarn), upgrading to $VERSION"
    note "$(tilde "$EXISTING")"
else
    ok "none"
fi

# ───────────────────────────────────────────────────────────── choose
if [ -z "$MODE" ]; then
    if [ "$INTERACTIVE" = 1 ]; then
        section "How would you like to install?"
        option 1 Default "~/.local/bin, added to PATH, with a self-test"
        option 2 Custom "choose the folder, PATH and self-test"
        option q Cancel "change nothing"
        echo
        while [ -z "$MODE" ]; do
            ask choice "Choice ${D}[1]${X}" 1
            case "$choice" in
                1 | default) MODE=default ;;
                2 | custom) MODE=custom ;;
                q | Q | quit | cancel) printf '\n  Cancelled. Nothing was changed.\n\n'; exit 0 ;;
                *) printf '  %sPick 1, 2 or q.%s\n' "$WARN" "$X" ;;
            esac
        done
    else
        MODE=default
    fi
fi

if [ "$MODE" = default ]; then
    BIN_DIR="$DEFAULT_DIR"
elif [ "$MODE" = custom ]; then
    section "Where should tarn go?"
    option 1 "~/.local/bin" "for you only, no sudo (recommended)"
    option 2 "/usr/local/bin" "for everyone, asks for your password"
    option 3 "~/.cargo/bin" "next to cargo and rustc"
    option 4 "Other" "type a folder"
    echo
    while [ -z "$BIN_DIR" ]; do
        ask where "Choice ${D}[1]${X}" 1
        case "$where" in
            1) BIN_DIR="$DEFAULT_DIR" ;;
            2) BIN_DIR="/usr/local/bin" ;;
            3) BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin" ;;
            4) ask BIN_DIR "Folder" "$DEFAULT_DIR" ;;
            *) printf '  %sPick 1 to 4.%s\n' "$WARN" "$X" ;;
        esac
    done
    echo
    if yes_no "Add it to your PATH if needed?" y; then EDIT_PATH=1; else EDIT_PATH=0; fi
    if yes_no "Run a self-test afterwards?" y; then SELF_TEST=1; else SELF_TEST=0; fi
fi

# Normalise: expand ~ and make absolute.
BIN_DIR="${BIN_DIR/#\~/$HOME}"
case "$BIN_DIR" in /*) ;; *) BIN_DIR="$PWD/$BIN_DIR" ;; esac
BIN_DIR="${BIN_DIR%/}"
TARGET="$BIN_DIR/tarn"

if [ "$MODE" = custom ]; then
    printf '\n    %-12s%s\n    %-12s%s\n    %-12s%s\n\n' "install to" "$(tilde "$TARGET")" \
        "PATH" "$([ $EDIT_PATH = 1 ] && echo "add if needed" || echo "leave alone")" \
        "self-test" "$([ $SELF_TEST = 1 ] && echo "yes" || echo "no")"
    yes_no "Install?" y || { printf '\n  Cancelled. Nothing was changed.\n\n'; exit 0; }
fi

# ───────────────────────────────────────────────────────────── install
section "Installing"

step "compile"
BUILD_START=$SECONDS
if spin "$TMPD/build.log" cargo build --release --manifest-path "$REPO/Cargo.toml"; then
    ok "done in $((SECONDS - BUILD_START))s"
else
    bad "failed"
    sed 's/^/                   /' "$TMPD/build.log" | tail -n 25
    die "The build failed (log above)."
fi

step "binary"
SUDO=""
if ! mkdir -p "$BIN_DIR" 2> /dev/null || [ ! -w "$BIN_DIR" ]; then
    SUDO="sudo"
    note "$BIN_DIR needs sudo"
    $SUDO mkdir -p "$BIN_DIR" || die "Could not create $BIN_DIR."
fi
$SUDO install -m 755 "$REPO/target/release/tarn" "$TARGET" || die "Could not write $TARGET."
ok "$(tilde "$TARGET")"

step "PATH"
RC=""
case ":$PATH:" in
    *":$BIN_DIR:"*) ok "already set up" ;;
    *)
        if [ "$EDIT_PATH" = 1 ]; then
            RC=$(shell_rc)
            add_path_block "$RC" "$BIN_DIR"
            ok "added to $(tilde "$RC")"
            note "open a new terminal, or run: source $(tilde "$RC")"
        else
            warn "not on PATH (left alone)"
            note "add it yourself: export PATH=\"$BIN_DIR:\$PATH\""
        fi
        ;;
esac
SHADOW=$(command -v tarn 2> /dev/null)
if [ -n "$SHADOW" ] && [ "$SHADOW" != "$TARGET" ]; then
    note "${WARN}note:${X}${D} $SHADOW comes first on your PATH"
fi

if [ "$SELF_TEST" = 1 ]; then
    step "self-test"
    printf 'fn main():\n    val words = ["hello", "tarn"]\n    print(f"{words[0]}, {words[1]}")\n' > "$TMPD/hello.tr"
    # `build` also warms the runtime cache, so your first real run is faster.
    if "$TARGET" build "$TMPD/hello.tr" -o "$TMPD/hello" > "$TMPD/test.log" 2>&1 && [ "$("$TMPD/hello")" = "hello, tarn" ]; then
        ok "compiled and ran a program"
    else
        bad "failed"
        sed 's/^/                   /' "$TMPD/test.log" | tail -n 10
        note "tarn is installed, but could not compile a program. Is clang installed?"
    fi
fi

step "manifest"
mkdir -p "$DATA_DIR"
cp "$REPO/uninstall.sh" "$DATA_DIR/uninstall.sh" && chmod +x "$DATA_DIR/uninstall.sh"
{
    echo "version=$VERSION"
    echo "installed=$(date '+%Y-%m-%d %H:%M:%S')"
    echo "source=$REPO"
    echo "bin=$TARGET"
    [ -n "$RC" ] && echo "rc=$RC"
    # remember earlier installs (e.g. another dir) so the uninstaller finds them
    if [ -f "$MANIFEST" ]; then
        grep -E '^(bin|rc)=' "$MANIFEST" | while IFS='=' read -r k v; do
            [ -e "$v" ] && echo "$k=$v"
        done
    fi
} | awk '!seen[$0]++' > "$TMPD/manifest" && cat "$TMPD/manifest" > "$MANIFEST"
ok "$(tilde "$MANIFEST")"

# ───────────────────────────────────────────────────────────── done
printf '\n  %sDone.%s tarn %s is installed.\n\n' "$B" "$X" "$VERSION"
printf '    tarn hello.tr                  %srun a script%s\n' "$D" "$X"
printf '    tarn build app.tr -o app       %sbuild a native program%s\n' "$D" "$X"
printf '    tarn --help                    %severything else%s\n' "$D" "$X"
printf '\n  %sTo remove it: %s/uninstall.sh%s\n\n' "$D" "$(tilde "$DATA_DIR")" "$X"
