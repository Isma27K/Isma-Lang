#!/usr/bin/env bash
#  ██╗███████╗███╗   ███╗ █████╗
#  ██║██╔════╝████╗ ████║██╔══██╗   uninstaller
#  ██║███████╗██╔████╔██║███████║
#  ██║╚════██║██║╚██╔╝██║██╔══██║   ./uninstall.sh       interactive: [1] wipe everything or [2] pick targets
#  ██║███████║██║ ╚═╝ ██║██║  ██║   ./uninstall.sh -y    wipe everything, no questions asked
#  ╚═╝╚══════╝╚═╝     ╚═╝╚═╝  ╚═╝
#
#  Flags: --keep-cache (leave ~/.cache/isma)  --no-anim (no animations)
#  Env:   NO_COLOR=1 disables colors, ISMA_NO_ANIM=1 disables animations.
#  Removes only what isma installed: binaries, compile cache, PATH entries
#  and ~/.local/share/isma. Your source code is never touched.

DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/isma"
MANIFEST="$DATA_DIR/manifest"
CACHE_DIR="${ISMA_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/isma}"

YES=0
KEEP_CACHE=0
ANIM=1

usage() { sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^#//'; }

while [ $# -gt 0 ]; do
    case "$1" in
        -y | --yes | --default) YES=1 ;;
        --keep-cache) KEEP_CACHE=1 ;;
        --no-anim) ANIM=0 ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

INTERACTIVE=0
[ -t 0 ] && [ -t 1 ] && INTERACTIVE=1
[ "$INTERACTIVE" = 0 ] && YES=1
[ -t 1 ] || ANIM=0
[ -n "${ISMA_NO_ANIM:-}" ] && ANIM=0

# ───────────────────────────────────────────────────────────── look & feel
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    R1=$'\033[38;5;196m' R2=$'\033[38;5;160m' R3=$'\033[38;5;124m' R4=$'\033[38;5;88m' R5=$'\033[38;5;52m'
    W=$'\033[1;97m' D=$'\033[2m' Y=$'\033[38;5;226m' G=$'\033[38;5;46m' X=$'\033[0m'
else
    R1="" R2="" R3="" R4="" R5="" W="" D="" Y="" G="" X=""
fi

pause() { [ "$ANIM" = 1 ] && sleep "$1"; return 0; }
cursor() { [ -t 1 ] && printf '\033[?25%s' "$1"; return 0; }
trap 'cursor h' EXIT
trap 'printf "\n\n  %s[--] aborted. nothing else was touched.%s\n\n" "$Y" "$X"; exit 130' INT

banner() {
    local l=(
        "██╗███████╗███╗   ███╗ █████╗ "
        "██║██╔════╝████╗ ████║██╔══██╗"
        "██║███████╗██╔████╔██║███████║"
        "██║╚════██║██║╚██╔╝██║██╔══██║"
        "██║███████║██║ ╚═╝ ██║██║  ██║"
        "╚═╝╚══════╝╚═╝     ╚═╝╚═╝  ╚═╝"
    )
    local c=("$R1" "$R1" "$R2" "$R3" "$R4" "$R5") i
    echo
    for i in 0 1 2 3 4 5; do
        printf '    %s%s%s\n' "${c[$i]}" "${l[$i]}" "$X"
        pause 0.04
    done
}

decode() {
    local text="$1" noise='01!@#$%&*<>/\|=+?ABCDEF' n=${#1} i j out ch
    if [ "$ANIM" = 0 ]; then printf '    %s%s%s\n' "$R1" "$text" "$X"; return; fi
    cursor l
    for ((i = 0; i <= n; i += 2)); do
        out=""
        for ((j = i; j < n; j++)); do
            ch="${text:j:1}"
            if [ "$ch" = " " ]; then out+=" "; else out+="${noise:RANDOM%${#noise}:1}"; fi
        done
        printf '\r    %s%s%s%s%s' "$R1" "${text:0:i}" "$R5" "$out" "$X"
        sleep 0.012
    done
    printf '\r    %s%s%s\n' "$R1" "$text" "$X"
    cursor h
}

STEP=0 PREFIX=""
step() {
    STEP=$((STEP + 1))
    local label="$1" dots
    dots=$(printf '%*s' $((32 - ${#label})) '' | tr ' ' '.')
    PREFIX=$(printf '  %s[0x%02X]%s %s %s%s%s ' "$R4" "$STEP" "$X" "$label" "$R5" "$dots" "$X")
    printf '%s' "$PREFIX"
    pause 0.06
}
hit()  { printf '%s%s%s\n' "$R1" "$1" "$X"; }
ok()   { printf '%s%s%s\n' "$G" "$1" "$X"; }
warn() { printf '%s%s%s\n' "$Y" "$1" "$X"; }
note() { printf '         %s└─ %s%s\n' "$D" "$1" "$X"; }
rule() { printf '\n  %s──[ %s%s%s ]%s%s\n\n' "$R5" "$R2" "$1" "$R5" "$(printf '%*s' $((40 - ${#1})) '' | sed 's/ /─/g')" "$X"; }

ask() {
    local var="$1" q="$2" def="$3" ans
    printf '  %s┌─[%sroot%s@%sisma%s]─[%s%s%s]\n  └──╼ %s#%s ' "$R4" "$R1" "$R4" "$R2" "$R4" "$W" "$q" "$R4" "$R1" "$X"
    read -r ans || ans=""
    [ -z "$ans" ] && ans="$def"
    printf -v "$var" '%s' "$ans"
    echo
}
option() { printf '    %s[%s%s%s]%s %-9s %s%s%s\n' "$R4" "$R1" "$1" "$R4" "$X" "$2" "$D" "$3" "$X"; }
tilde() { case "$1" in "$HOME"/*) echo "~${1#"$HOME"}" ;; *) echo "$1" ;; esac; }

# A directory is only wiped if everything in it looks like something isma made.
only_contains() {
    local dir="$1" pattern="$2" f
    case "$dir" in "" | "/" | "$HOME" | "$HOME/") return 1 ;; esac
    for f in "$dir"/* "$dir"/.[!.]*; do
        [ -e "$f" ] || continue
        echo "$(basename "$f")" | grep -Eqx "$pattern" || return 1
    done
    return 0
}

is_isma() { [ -x "$1" ] && "$1" --version 2> /dev/null | grep -q '^isma '; }

remove_path_block() {
    local rc="$1" tmp
    tmp=$(mktemp)
    awk '/^# >>> isma >>>$/{skip=1} !skip{print} /^# <<< isma <<<$/{skip=0}' "$rc" > "$tmp" && cat "$tmp" > "$rc"
    rm -f "$tmp"
}

# ───────────────────────────────────────────────────────────── recon
banner
decode ">> self-destruct protocol // trace removal"
printf '    %suninstaller :: %s%s\n' "$D" "$(uname -sm)" "$X"
rule "recon"

# targets: parallel arrays (bash 3.2 friendly)
KIND=() TPATH=() INFO=()
add_target() { KIND+=("$1"); TPATH+=("$2"); INFO+=("$3"); }
seen() { local p; for p in ${TPATH[@]+"${TPATH[@]}"}; do [ "$p" = "$1" ] && return 0; done; return 1; }

step "reading install manifest"
SOURCE=""
if [ -f "$MANIFEST" ]; then
    SOURCE=$(sed -n 's/^source=//p' "$MANIFEST" | head -n 1)
    hit "found ($(sed -n 's/^version=/v/p' "$MANIFEST" | head -n 1), installed $(sed -n 's/^installed=//p' "$MANIFEST" | head -n 1))"
else
    warn "none, scanning manually"
fi

step "scanning for isma binaries"
CANDIDATES=""
[ -f "$MANIFEST" ] && CANDIDATES=$(sed -n 's/^bin=//p' "$MANIFEST")
CANDIDATES="$CANDIDATES
$(type -ap isma 2> /dev/null)
$HOME/.local/bin/isma
/usr/local/bin/isma
${CARGO_HOME:-$HOME/.cargo}/bin/isma
/opt/homebrew/bin/isma"
n=0 foreign=""
while IFS= read -r b; do
    [ -n "$b" ] && [ -e "$b" ] || continue
    seen "$b" && continue
    case " $foreign " in *" $b "*) continue ;; esac
    if is_isma "$b"; then
        add_target binary "$b" "$("$b" --version 2> /dev/null)"
        n=$((n + 1))
    else
        foreign="$foreign $b"
    fi
done <<< "$CANDIDATES"
if [ $n -gt 0 ]; then hit "$n found"; else ok "none"; fi
for f in $foreign; do note "skipping $f: not an isma binary"; done

step "scanning compile cache"
if [ "$KEEP_CACHE" = 1 ]; then
    ok "kept (--keep-cache)"
elif [ -d "$CACHE_DIR" ]; then
    if only_contains "$CACHE_DIR" 'bin|runtime-[0-9a-f]+\.o|\.tmp-.*'; then
        add_target cache "$CACHE_DIR" "$(du -sh "$CACHE_DIR" 2> /dev/null | cut -f1) of compiled programs"
        hit "$(du -sh "$CACHE_DIR" 2> /dev/null | cut -f1)"
    else
        warn "skipped"
        note "$CACHE_DIR contains files isma did not create"
    fi
else
    ok "none"
fi

step "scanning shell configs"
n=0
RCS="$([ -f "$MANIFEST" ] && sed -n 's/^rc=//p' "$MANIFEST")
$HOME/.bashrc
$HOME/.bash_profile
$HOME/.profile
${ZDOTDIR:-$HOME}/.zshrc"
while IFS= read -r rc; do
    [ -n "$rc" ] && [ -f "$rc" ] || continue
    seen "$rc" && continue
    case "$rc" in *.fish) continue ;; esac
    if grep -qx '# >>> isma >>>' "$rc"; then
        add_target path "$rc" "PATH entry"
        n=$((n + 1))
    fi
done <<< "$RCS"
FISH="${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/isma.fish"
if [ -f "$FISH" ] && grep -q 'added by the isma installer' "$FISH"; then
    add_target path "$FISH" "fish PATH entry"
    n=$((n + 1))
fi
if [ $n -gt 0 ]; then hit "$n PATH entr$([ $n = 1 ] && echo y || echo ies)"; else ok "clean"; fi

step "scanning data dir"
if [ -d "$DATA_DIR" ] && only_contains "$DATA_DIR" 'manifest|uninstall\.sh'; then
    add_target data "$DATA_DIR" "manifest + uninstaller copy"
    hit "found"
elif [ -d "$DATA_DIR" ]; then
    warn "skipped"
    note "$DATA_DIR contains files isma did not create"
else
    ok "none"
fi

TOTAL=${#KIND[@]}
if [ "$TOTAL" = 0 ]; then
    rule "result"
    printf '  %s[ok] no trace of isma on this system. nothing to wipe.%s\n\n' "$G" "$X"
    exit 0
fi

rule "targets acquired"
for ((i = 0; i < TOTAL; i++)); do
    printf '    %s[%s%d%s]%s %-7s %-40s %s%s%s\n' "$R4" "$R1" $((i + 1)) "$R4" "$X" "${KIND[$i]}" "$(tilde "${TPATH[$i]}")" "$D" "${INFO[$i]}" "$X"
done
echo

# ───────────────────────────────────────────────────────────── choose
SELECTED=()
if [ "$YES" = 1 ]; then
    for ((i = 0; i < TOTAL; i++)); do SELECTED+=("$i"); done
else
    option 1 default "wipe every target above"
    option 2 custom "pick which targets to wipe"
    option q abort "get out, change nothing"
    echo
    MODE=""
    while [ -z "$MODE" ]; do
        ask choice "mode" 1
        case "$choice" in
            1 | default) MODE=default ;;
            2 | custom) MODE=custom ;;
            q | Q | quit | abort) printf '  %s[--] aborted. nothing was changed.%s\n\n' "$D" "$X"; exit 0 ;;
            *) printf '  %s[!!] pick 1, 2 or q%s\n\n' "$Y" "$X" ;;
        esac
    done
    if [ "$MODE" = default ]; then
        for ((i = 0; i < TOTAL; i++)); do SELECTED+=("$i"); done
    else
        while [ ${#SELECTED[@]} = 0 ]; do
            ask picks "targets to wipe, e.g. 1 3" ""
            for p in $picks; do
                if [[ "$p" =~ ^[0-9]+$ ]] && [ "$p" -ge 1 ] && [ "$p" -le "$TOTAL" ]; then
                    SELECTED+=($((p - 1)))
                else
                    printf '  %s[!!] ignoring "%s": not a target number%s\n' "$Y" "$p" "$X"
                fi
            done
            [ ${#SELECTED[@]} = 0 ] && printf '  %s[!!] choose at least one target (1-%d)%s\n\n' "$Y" "$TOTAL" "$X"
        done
    fi
    ask confirm "wipe ${#SELECTED[@]} target(s)? this cannot be undone [y/N]" n
    case "$confirm" in y | Y | yes | YES) ;; *) printf '  %s[--] aborted. nothing was changed.%s\n\n' "$D" "$X"; exit 0 ;; esac
fi

# ───────────────────────────────────────────────────────────── wipe
rule "wipe"
if [ "$ANIM" = 1 ]; then
    cursor l
    for t in 3 2 1; do
        printf '\r  %sself-destruct in %s%s%s...%s' "$R3" "$R1" "$t" "$R3" "$X"
        sleep 0.4
    done
    printf '\r\033[K'
    cursor h
fi

shred_bar() {   # purely cosmetic progress bar, then the real action runs
    [ "$ANIM" = 1 ] || return 0
    local i
    cursor l
    for i in 1 2 3 4 5 6 7 8 9 10; do
        printf '\r%s%s%s%s%s' "$PREFIX" "$R1" "$(printf '%*s' $i '' | sed 's/ /█/g')" "$R5" "$(printf '%*s' $((10 - i)) '' | sed 's/ /░/g')"
        sleep 0.025
    done
    printf '\r\033[K%s' "$PREFIX"
    cursor h
}

FAILED=0 PATH_CHANGED=0
for i in "${SELECTED[@]}"; do
    kind="${KIND[$i]}" target="${TPATH[$i]}"
    step "shredding $kind"
    shred_bar
    case "$kind" in
        binary)
            if [ -w "$(dirname "$target")" ]; then rm -f "$target"; else sudo rm -f "$target"; fi
            ;;
        cache | data)
            rm -rf "$target"
            ;;
        path)
            case "$target" in
                *.fish) rm -f "$target" ;;
                *) remove_path_block "$target" ;;
            esac
            PATH_CHANGED=1
            ;;
    esac
    if [ -e "$target" ] && [ "$kind" != path ]; then
        warn "could not remove $(tilde "$target")"
        FAILED=1
    elif [ "$kind" = path ] && [ -f "$target" ] && grep -qx '# >>> isma >>>' "$target"; then
        warn "could not edit $(tilde "$target")"
        FAILED=1
    elif [ "$kind" = path ]; then
        hit "removed PATH entry from $(tilde "$target")"
    else
        hit "wiped $(tilde "$target")"
    fi
done

# ───────────────────────────────────────────────────────────── done
echo
if [ "$FAILED" = 1 ]; then
    line="PARTIAL WIPE :: some targets survived (see above)"
elif [ ${#SELECTED[@]} -lt "$TOTAL" ]; then
    line="TARGETS WIPED :: ${#SELECTED[@]} of $TOTAL removed, the rest left intact"
else
    line="TRACE ERASED :: isma has left the building"
fi
bar=$(printf '%*s' $((${#line} + 4)) '' | sed 's/ /═/g')
printf '  %s╔%s╗%s\n' "$R2" "$bar" "$X"
printf '  %s║  %s%s%s  ║%s\n' "$R2" "$W" "$line" "$R2" "$X"
printf '  %s╚%s╝%s\n' "$R2" "$bar" "$X"
echo
[ "$PATH_CHANGED" = 1 ] && note "open a new terminal so your shell forgets the old PATH"
if [ -n "$SOURCE" ] && [ -d "$SOURCE" ]; then
    note "source code at $(tilde "$SOURCE") was not touched. reinstall: $(tilde "$SOURCE")/install.sh"
fi
echo
exit $FAILED
