#!/usr/bin/env bash
#  tarn uninstaller
#
#    ./uninstall.sh       interactive: remove everything, or pick what to remove
#    ./uninstall.sh -y    remove everything, no questions asked
#
#  Flags: --keep-cache (leave ~/.cache/tarn)  --no-anim (accepted, for symmetry)
#  Env:   NO_COLOR=1 disables colors.
#  Removes only what tarn installed: binaries, the compile cache, PATH entries
#  and ~/.local/share/tarn. Your source code is never touched.

DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/tarn"
MANIFEST="$DATA_DIR/manifest"
CACHE_DIR="${TARN_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/tarn}"

YES=0
KEEP_CACHE=0

usage() { sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^#//'; }

while [ $# -gt 0 ]; do
    case "$1" in
        -y | --yes | --default) YES=1 ;;
        --keep-cache) KEEP_CACHE=1 ;;
        --no-anim) ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

INTERACTIVE=0
[ -t 0 ] && [ -t 1 ] && INTERACTIVE=1
[ "$INTERACTIVE" = 0 ] && YES=1
TTY=0
[ -t 1 ] && TTY=1

# ───────────────────────────────────────────────────────────── look
if [ "$TTY" = 1 ] && [ -z "${NO_COLOR:-}" ]; then
    B=$'\033[1m' D=$'\033[2m' OK=$'\033[32m' WARN=$'\033[33m' ERR=$'\033[31m' X=$'\033[0m'
else
    B="" D="" OK="" WARN="" ERR="" X=""
fi

trap 'printf "\n\n  Cancelled. Nothing else was changed.\n\n"; exit 130' INT

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
note() { printf '                   %s%s%s\n' "$D" "$1" "$X"; }
section() { printf '\n  %s%s%s\n' "$B" "$1" "$X"; }

ask() {
    local var="$1" q="$2" def="$3" ans
    printf '  %s %s›%s ' "$q" "$D" "$X"
    read -r ans || ans=""
    [ -z "$ans" ] && ans="$def"
    printf -v "$var" '%s' "$ans"
}
option() { printf '    %s%s%s  %-14s %s%s%s\n' "$B" "$1" "$X" "$2" "$D" "$3" "$X"; }
tilde() { case "$1" in "$HOME"/*) echo "~${1#"$HOME"}" ;; *) echo "$1" ;; esac; }

# A directory is only removed if everything in it looks like something tarn made.
only_contains() {
    local dir="$1" pattern="$2" f
    case "$dir" in "" | "/" | "$HOME" | "$HOME/") return 1 ;; esac
    for f in "$dir"/* "$dir"/.[!.]*; do
        [ -e "$f" ] || continue
        echo "$(basename "$f")" | grep -Eqx "$pattern" || return 1
    done
    return 0
}

is_tarn() { [ -x "$1" ] && "$1" --version 2> /dev/null | grep -q '^tarn '; }

remove_path_block() {
    local rc="$1" tmp
    tmp=$(mktemp)
    awk '/^# >>> tarn >>>$/{skip=1} !skip{print} /^# <<< tarn <<<$/{skip=0}' "$rc" > "$tmp" && cat "$tmp" > "$rc"
    rm -f "$tmp"
}

# ───────────────────────────────────────────────────────────── look around
printf '\n  %starn%s %s· uninstaller%s\n' "$B" "$X" "$D" "$X"
section "Looking"

# targets: parallel arrays (bash 3.2 friendly)
KIND=() TPATH=() INFO=()
add_target() { KIND+=("$1"); TPATH+=("$2"); INFO+=("$3"); }
seen() { local p; for p in ${TPATH[@]+"${TPATH[@]}"}; do [ "$p" = "$1" ] && return 0; done; return 1; }

step "manifest"
SOURCE=""
if [ -f "$MANIFEST" ]; then
    SOURCE=$(sed -n 's/^source=//p' "$MANIFEST" | head -n 1)
    ok "$(sed -n 's/^version=/v/p' "$MANIFEST" | head -n 1), installed $(sed -n 's/^installed=//p' "$MANIFEST" | head -n 1)"
else
    ok "none, searching instead"
fi

step "binaries"
CANDIDATES=""
[ -f "$MANIFEST" ] && CANDIDATES=$(sed -n 's/^bin=//p' "$MANIFEST")
CANDIDATES="$CANDIDATES
$(type -ap tarn 2> /dev/null)
$HOME/.local/bin/tarn
/usr/local/bin/tarn
${CARGO_HOME:-$HOME/.cargo}/bin/tarn
/opt/homebrew/bin/tarn"
n=0 foreign=""
while IFS= read -r b; do
    [ -n "$b" ] && [ -e "$b" ] || continue
    seen "$b" && continue
    case " $foreign " in *" $b "*) continue ;; esac
    if is_tarn "$b"; then
        add_target binary "$b" "$("$b" --version 2> /dev/null)"
        n=$((n + 1))
    else
        foreign="$foreign $b"
    fi
done <<< "$CANDIDATES"
if [ $n -gt 0 ]; then ok "$n found"; else ok "none"; fi
for f in $foreign; do note "leaving $f alone: it isn't tarn"; done

step "cache"
if [ "$KEEP_CACHE" = 1 ]; then
    ok "kept (--keep-cache)"
elif [ -d "$CACHE_DIR" ]; then
    if only_contains "$CACHE_DIR" 'bin|runtime-[0-9a-f]+\.o|\.tmp-.*'; then
        add_target cache "$CACHE_DIR" "$(du -sh "$CACHE_DIR" 2> /dev/null | cut -f1) of compiled programs"
        ok "$(du -sh "$CACHE_DIR" 2> /dev/null | cut -f1)"
    else
        warn "skipped"
        note "$CACHE_DIR has files tarn did not create"
    fi
else
    ok "none"
fi

step "PATH"
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
    if grep -qx '# >>> tarn >>>' "$rc"; then
        add_target path "$rc" "PATH entry"
        n=$((n + 1))
    fi
done <<< "$RCS"
FISH="${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/tarn.fish"
if [ -f "$FISH" ] && grep -q 'added by the tarn installer' "$FISH"; then
    add_target path "$FISH" "fish PATH entry"
    n=$((n + 1))
fi
if [ $n -gt 0 ]; then ok "$n entr$([ $n = 1 ] && echo y || echo ies)"; else ok "none"; fi

step "data"
if [ -d "$DATA_DIR" ] && only_contains "$DATA_DIR" 'manifest|uninstall\.sh'; then
    add_target data "$DATA_DIR" "manifest and this uninstaller"
    ok "found"
elif [ -d "$DATA_DIR" ]; then
    warn "skipped"
    note "$DATA_DIR has files tarn did not create"
else
    ok "none"
fi

TOTAL=${#KIND[@]}
if [ "$TOTAL" = 0 ]; then
    printf '\n  Nothing to remove: tarn is not installed here.\n\n'
    exit 0
fi

section "Found"
for ((i = 0; i < TOTAL; i++)); do
    printf '    %s%d%s  %-8s %-38s %s%s%s\n' "$B" $((i + 1)) "$X" "${KIND[$i]}" "$(tilde "${TPATH[$i]}")" "$D" "${INFO[$i]}" "$X"
done

# ───────────────────────────────────────────────────────────── choose
SELECTED=()
if [ "$YES" = 1 ]; then
    for ((i = 0; i < TOTAL; i++)); do SELECTED+=("$i"); done
else
    section "What should go?"
    option 1 Everything "all of the above"
    option 2 "Let me pick" "choose by number"
    option q Cancel "change nothing"
    echo
    MODE=""
    while [ -z "$MODE" ]; do
        ask choice "Choice ${D}[1]${X}" 1
        case "$choice" in
            1) MODE=all ;;
            2) MODE=pick ;;
            q | Q | quit | cancel) printf '\n  Cancelled. Nothing was changed.\n\n'; exit 0 ;;
            *) printf '  %sPick 1, 2 or q.%s\n' "$WARN" "$X" ;;
        esac
    done
    if [ "$MODE" = all ]; then
        for ((i = 0; i < TOTAL; i++)); do SELECTED+=("$i"); done
    else
        while [ ${#SELECTED[@]} = 0 ]; do
            ask picks "Numbers, e.g. 1 3" ""
            for p in $picks; do
                if [[ "$p" =~ ^[0-9]+$ ]] && [ "$p" -ge 1 ] && [ "$p" -le "$TOTAL" ]; then
                    SELECTED+=($((p - 1)))
                else
                    printf '  %sIgnoring "%s": not one of the numbers.%s\n' "$WARN" "$p" "$X"
                fi
            done
            [ ${#SELECTED[@]} = 0 ] && printf '  %sChoose at least one (1-%d).%s\n' "$WARN" "$TOTAL" "$X"
        done
    fi
    ask confirm "Remove ${#SELECTED[@]} item(s)? This can't be undone. ${D}[y/N]${X}" n
    case "$confirm" in y | Y | yes | YES) ;; *) printf '\n  Cancelled. Nothing was changed.\n\n'; exit 0 ;; esac
fi

# ───────────────────────────────────────────────────────────── remove
section "Removing"
FAILED=0 PATH_CHANGED=0
for i in "${SELECTED[@]}"; do
    kind="${KIND[$i]}" target="${TPATH[$i]}"
    step "$kind"
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
    elif [ "$kind" = path ] && [ -f "$target" ] && grep -qx '# >>> tarn >>>' "$target"; then
        warn "could not edit $(tilde "$target")"
        FAILED=1
    elif [ "$kind" = path ]; then
        ok "removed from $(tilde "$target")"
    else
        ok "removed $(tilde "$target")"
    fi
done

# ───────────────────────────────────────────────────────────── done
echo
if [ "$FAILED" = 1 ]; then
    printf '  %sSome items could not be removed%s (see above).\n' "$WARN" "$X"
elif [ ${#SELECTED[@]} -lt "$TOTAL" ]; then
    printf '  %sDone.%s Removed %d of %d items; the rest are untouched.\n' "$B" "$X" "${#SELECTED[@]}" "$TOTAL"
else
    printf '  %sDone.%s tarn is uninstalled.\n' "$B" "$X"
fi
[ "$PATH_CHANGED" = 1 ] && printf '  %sOpen a new terminal so your shell forgets the old PATH.%s\n' "$D" "$X"
if [ -n "$SOURCE" ] && [ -d "$SOURCE" ]; then
    printf '  %sThe source at %s was not touched. Reinstall with %s/install.sh%s\n' "$D" "$(tilde "$SOURCE")" "$(tilde "$SOURCE")" "$X"
fi
echo
exit $FAILED
