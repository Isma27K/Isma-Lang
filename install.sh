#!/usr/bin/env bash
#  ██╗███████╗███╗   ███╗ █████╗
#  ██║██╔════╝████╗ ████║██╔══██╗   installer
#  ██║███████╗██╔████╔██║███████║
#  ██║╚════██║██║╚██╔╝██║██╔══██║   ./install.sh            interactive: [1] default or [2] custom
#  ██║███████║██║ ╚═╝ ██║██║  ██║   ./install.sh -y         default install, no questions asked
#  ╚═╝╚══════╝╚═╝     ╚═╝╚═╝  ╚═╝   ./install.sh --dir DIR  install into DIR, no questions asked
#
#  Flags: --no-path (never edit shell config)  --no-test (skip self-test)
#         --no-anim (no animations)            --uninstall (run ./uninstall.sh)
#  Env:   NO_COLOR=1 disables colors, ISMA_NO_ANIM=1 disables animations.

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/isma"
MANIFEST="$DATA_DIR/manifest"
DEFAULT_DIR="$HOME/.local/bin"

MODE=""        # default | custom | cli
BIN_DIR=""
EDIT_PATH=1
SELF_TEST=1
ANIM=1

usage() { sed -n '2,11p' "${BASH_SOURCE[0]}" | sed 's/^#//'; }

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
[ -t 1 ] || ANIM=0
[ -n "${ISMA_NO_ANIM:-}" ] && ANIM=0

# ───────────────────────────────────────────────────────────── look & feel
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    G1=$'\033[38;5;46m' G2=$'\033[38;5;40m' G3=$'\033[38;5;34m' G4=$'\033[38;5;28m' G5=$'\033[38;5;22m'
    W=$'\033[1;97m' D=$'\033[2m' Y=$'\033[38;5;226m' R=$'\033[38;5;196m' C=$'\033[38;5;51m' X=$'\033[0m'
else
    G1="" G2="" G3="" G4="" G5="" W="" D="" Y="" R="" C="" X=""
fi

pause() { [ "$ANIM" = 1 ] && sleep "$1"; return 0; }
cursor() { [ -t 1 ] && printf '\033[?25%s' "$1"; return 0; }   # h = show, l = hide

BG_PID="" TMPD=""
cleanup() {
    [ -n "$BG_PID" ] && kill "$BG_PID" 2>/dev/null
    [ -n "$TMPD" ] && rm -rf "$TMPD"
    cursor h
}
trap cleanup EXIT
trap 'printf "\n\n  %s[XX] aborted by user. nothing else was touched.%s\n\n" "$R" "$X"; exit 130' INT

banner() {
    local l=(
        "██╗███████╗███╗   ███╗ █████╗ "
        "██║██╔════╝████╗ ████║██╔══██╗"
        "██║███████╗██╔████╔██║███████║"
        "██║╚════██║██║╚██╔╝██║██╔══██║"
        "██║███████║██║ ╚═╝ ██║██║  ██║"
        "╚═╝╚══════╝╚═╝     ╚═╝╚═╝  ╚═╝"
    )
    local c=("$G1" "$G1" "$G2" "$G3" "$G4" "$G5") i
    echo
    for i in 0 1 2 3 4 5; do
        printf '    %s%s%s\n' "${c[$i]}" "${l[$i]}" "$X"
        pause 0.04
    done
}

# Text that "decrypts" itself from noise, left to right.
decode() {
    local text="$1" color="${2:-$G1}" noise='01!@#$%&*<>/\|=+?ABCDEFabcdef' n=${#1} i j out ch
    if [ "$ANIM" = 0 ]; then printf '    %s%s%s\n' "$color" "$text" "$X"; return; fi
    cursor l
    for ((i = 0; i <= n; i += 2)); do
        out="${text:0:i}"
        for ((j = i; j < n; j++)); do
            ch="${text:j:1}"
            if [ "$ch" = " " ]; then out+=" "; else out+="${noise:RANDOM%${#noise}:1}"; fi
        done
        printf '\r    %s%s%s%s%s' "$color" "${text:0:i}" "$G5" "${out:i}" "$X"
        sleep 0.012
    done
    printf '\r    %s%s%s\n' "$color" "$text" "$X"
    cursor h
}

STEP=0 PREFIX=""
step() {
    STEP=$((STEP + 1))
    local label="$1" dots
    dots=$(printf '%*s' $((32 - ${#label})) '' | tr ' ' '.')
    PREFIX=$(printf '  %s[0x%02X]%s %s %s%s%s ' "$G4" "$STEP" "$X" "$label" "$G5" "$dots" "$X")
    printf '%s' "$PREFIX"
    pause 0.06
}
ok()   { printf '%s%s%s\n' "$G1" "$1" "$X"; }
warn() { printf '%s%s%s\n' "$Y" "$1" "$X"; }
bad()  { printf '%s%s%s\n' "$R" "$1" "$X"; }
note() { printf '         %s└─ %s%s\n' "$D" "$1" "$X"; }
die()  { printf '\n  %s[XX] %s%s\n\n' "$R" "$1" "$X"; exit 1; }
tilde() { case "$1" in "$HOME"/*) echo "~${1#"$HOME"}" ;; *) echo "$1" ;; esac; }
rule() { printf '\n  %s──[ %s%s%s ]%s%s\n\n' "$G5" "$G2" "$1" "$G5" "$(printf '%*s' $((40 - ${#1})) '' | sed 's/ /─/g')" "$X"; }

# Run a command in the background with a spinner showing its latest output.
spin() {
    local log="$1" i=0 start=$SECONDS last
    shift
    "$@" > "$log" 2>&1 &
    BG_PID=$!
    if [ "$ANIM" = 1 ]; then
        local f=(⣾ ⣽ ⣻ ⢿ ⡿ ⣟ ⣯ ⣷)
        cursor l
        while kill -0 "$BG_PID" 2> /dev/null; do
            last=$(tail -n 1 "$log" 2> /dev/null | tr -d '\r' | sed 's/^ *//' | cut -c1-34)
            printf '\r\033[K%s%s%s%s %s%2ds%s %s%s%s' "$PREFIX" "$G1" "${f[$((i % 8))]}" "$X" "$D" $((SECONDS - start)) "$X" "$G4" "$last" "$X"
            i=$((i + 1))
            sleep 0.08
        done
        printf '\r\033[K%s' "$PREFIX"
        cursor h
    fi
    wait "$BG_PID"
    local status=$?
    BG_PID=""
    return $status
}

# A hacker-style prompt:  ┌─[isma@installer]─[question]
#                         └──╼ $
ask() {
    local var="$1" q="$2" def="$3" ans
    printf '  %s┌─[%sisma%s@%sinstaller%s]─[%s%s%s]\n  └──╼ %s$%s ' "$G4" "$G1" "$G4" "$G2" "$G4" "$W" "$q" "$G4" "$G1" "$X"
    read -r ans || ans=""
    [ -z "$ans" ] && ans="$def"
    printf -v "$var" '%s' "$ans"
    echo
}
yes_no() {
    local a hint="y/N"
    [ "$2" = y ] && hint="Y/n"
    ask a "$1 [$hint]" "$2"
    case "$a" in y | Y | yes | YES) return 0 ;; *) return 1 ;; esac
}
option() { printf '    %s[%s%s%s]%s %-15s %s%s%s\n' "$G4" "$G1" "$1" "$G4" "$X" "$2" "$D" "$3" "$X"; }

find_clang() {
    if [ -n "${ISMA_CC:-}" ]; then echo "$ISMA_CC"; return; fi
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
        fish) echo "${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/isma.fish" ;;
        *) echo "$HOME/.profile" ;;
    esac
}

remove_path_block() {
    local rc="$1" tmp
    [ -f "$rc" ] || return 0
    tmp=$(mktemp)
    awk '/^# >>> isma >>>$/{skip=1} !skip{print} /^# <<< isma <<<$/{skip=0}' "$rc" > "$tmp" && cat "$tmp" > "$rc"
    rm -f "$tmp"
}

add_path_block() {
    local rc="$1" dir="$2"
    mkdir -p "$(dirname "$rc")"
    case "$rc" in
        *.fish) printf '# added by the isma installer\nfish_add_path "%s"\n' "$dir" > "$rc" ;;
        *)
            remove_path_block "$rc"
            if [ -s "$rc" ] && [ -n "$(tail -c 1 "$rc")" ]; then echo >> "$rc"; fi
            printf '# >>> isma >>>\nexport PATH="%s:$PATH"\n# <<< isma <<<\n' "$dir" >> "$rc"
            ;;
    esac
}

# ───────────────────────────────────────────────────────────── recon
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -n 1)
TMPD=$(mktemp -d)

banner
decode ">> python syntax // kotlin types // native speed"
printf '    %sinstaller for isma v%s :: %s%s\n' "$D" "$VERSION" "$(uname -sm)" "$X"
rule "recon"

step "probing host"
ok "$(uname -s) $(uname -m) / $(basename "${SHELL:-sh}")"

step "locating rust toolchain"
if command -v cargo > /dev/null 2>&1; then
    ok "$(cargo --version | cut -d' ' -f1-2)"
else
    bad "not found"
    die "isma is built with cargo. install rust first: https://rustup.rs"
fi

step "locating clang backend"
CLANG=$(find_clang)
if [ -n "$CLANG" ]; then
    ok "clang $("$CLANG" --version 2> /dev/null | head -n 1 | sed 's/.*clang version \([0-9.]*\).*/\1/')"
    note "$(tilde "$CLANG")"
else
    warn "not found"
    note "isma needs clang to turn your code into native binaries:"
    note "apt install clang | dnf install clang | pacman -S clang | xcode-select --install"
fi

step "checking for existing isma"
EXISTING=$(command -v isma 2> /dev/null)
if [ -n "$EXISTING" ]; then
    warn "$("$EXISTING" --version 2> /dev/null || echo isma) at $EXISTING"
    note "it will be upgraded to v$VERSION"
else
    ok "none, fresh install"
fi

# ───────────────────────────────────────────────────────────── choose
if [ -z "$MODE" ]; then
    if [ "$INTERACTIVE" = 1 ]; then
        rule "select mode"
        option 1 default "install to ~/.local/bin, wire PATH, run self-test"
        option 2 custom "pick install dir, PATH wiring and self-test yourself"
        option q abort "get out, change nothing"
        echo
        while [ -z "$MODE" ]; do
            ask choice "mode" 1
            case "$choice" in
                1 | default) MODE=default ;;
                2 | custom) MODE=custom ;;
                q | Q | quit | abort) printf '  %s[--] aborted. nothing was changed.%s\n\n' "$D" "$X"; exit 0 ;;
                *) printf '  %s[!!] pick 1, 2 or q%s\n\n' "$Y" "$X" ;;
            esac
        done
    else
        MODE=default
    fi
fi

if [ "$MODE" = default ]; then
    BIN_DIR="$DEFAULT_DIR"
elif [ "$MODE" = custom ]; then
    rule "custom install"
    option 1 "~/.local/bin" "user-level, no sudo (recommended)"
    option 2 "/usr/local/bin" "system-wide, asks for your sudo password"
    option 3 "~/.cargo/bin" "next to cargo and rustc"
    option 4 "other" "type any directory"
    echo
    while [ -z "$BIN_DIR" ]; do
        ask where "install dir" 1
        case "$where" in
            1) BIN_DIR="$DEFAULT_DIR" ;;
            2) BIN_DIR="/usr/local/bin" ;;
            3) BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin" ;;
            4) ask BIN_DIR "path" "$DEFAULT_DIR" ;;
            *) printf '  %s[!!] pick 1-4%s\n\n' "$Y" "$X" ;;
        esac
    done
    if yes_no "add to PATH automatically if needed?" y; then EDIT_PATH=1; else EDIT_PATH=0; fi
    if yes_no "run a self-test after install?" y; then SELF_TEST=1; else SELF_TEST=0; fi
fi

# Normalise: expand ~ and make absolute.
BIN_DIR="${BIN_DIR/#\~/$HOME}"
case "$BIN_DIR" in /*) ;; *) BIN_DIR="$PWD/$BIN_DIR" ;; esac
BIN_DIR="${BIN_DIR%/}"
TARGET="$BIN_DIR/isma"

if [ "$MODE" = custom ]; then
    printf '  %spayload%s   isma v%s\n  %starget%s    %s\n  %sPATH%s      %s\n  %sself-test%s %s\n\n' \
        "$G4" "$X" "$VERSION" "$G4" "$X" "$(tilde "$TARGET")" "$G4" "$X" "$([ $EDIT_PATH = 1 ] && echo "auto" || echo "leave alone")" \
        "$G4" "$X" "$([ $SELF_TEST = 1 ] && echo "yes" || echo "no")"
    yes_no "deploy?" y || { printf '  %s[--] aborted. nothing was changed.%s\n\n' "$D" "$X"; exit 0; }
fi

# ───────────────────────────────────────────────────────────── deploy
rule "deploy"

step "compiling the isma compiler"
BUILD_START=$SECONDS
if spin "$TMPD/build.log" cargo build --release --manifest-path "$REPO/Cargo.toml"; then
    ok "done in $((SECONDS - BUILD_START))s"
else
    bad "failed"
    sed 's/^/         /' "$TMPD/build.log" | tail -n 25
    die "build failed (log above)"
fi

step "deploying binary"
SUDO=""
if ! mkdir -p "$BIN_DIR" 2> /dev/null || [ ! -w "$BIN_DIR" ]; then
    SUDO="sudo"
    warn "root needed"
    note "asking sudo to write into $BIN_DIR"
    $SUDO mkdir -p "$BIN_DIR" || die "could not create $BIN_DIR"
    printf '%s' "$PREFIX"
fi
$SUDO install -m 755 "$REPO/target/release/isma" "$TARGET" || die "could not write $TARGET"
ok "$(tilde "$TARGET")"

step "wiring PATH"
RC=""
case ":$PATH:" in
    *":$BIN_DIR:"*) ok "already on PATH" ;;
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
SHADOW=$(command -v isma 2> /dev/null)
if [ -n "$SHADOW" ] && [ "$SHADOW" != "$TARGET" ]; then
    note "${Y}heads up:${X}${D} $SHADOW comes first on your PATH and shadows this install"
fi

if [ "$SELF_TEST" = 1 ]; then
    step "self-test: hello world"
    printf 'fn main():\n    val words = ["isma", "online"]\n    print(f"{words[0]} {words[1]}")\n' > "$TMPD/hello.isma"
    # `build` also warms the runtime cache, so your first real run is faster.
    if "$TARGET" build "$TMPD/hello.isma" -o "$TMPD/hello" > "$TMPD/test.log" 2>&1 && [ "$("$TMPD/hello")" = "isma online" ]; then
        ok "compiled + executed natively"
    else
        bad "failed"
        sed 's/^/         /' "$TMPD/test.log" | tail -n 10
        note "isma is installed, but could not compile a program. is clang installed?"
    fi
fi

step "writing install manifest"
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
echo
pause 0.1
line="ACCESS GRANTED :: isma v$VERSION is live"
bar=$(printf '%*s' $((${#line} + 4)) '' | sed 's/ /═/g')
printf '  %s╔%s╗%s\n' "$G2" "$bar" "$X"
printf '  %s║  %s%s%s  ║%s\n' "$G2" "$W" "$line" "$G2" "$X"
printf '  %s╚%s╝%s\n' "$G2" "$bar" "$X"
echo
printf '  %snext moves%s\n' "$G3" "$X"
printf '    %s$%s isma hello.isma               %s# run a script, like python%s\n' "$G1" "$X" "$D" "$X"
printf '    %s$%s isma build app.isma -o app    %s# ship a native binary%s\n' "$G1" "$X" "$D" "$X"
printf '    %s$%s isma --help                   %s# everything else%s\n' "$G1" "$X" "$D" "$X"
printf '    %s$%s %s/uninstall.sh  %s# remove isma%s\n' "$G1" "$X" "$(tilde "$DATA_DIR")" "$D" "$X"
echo
