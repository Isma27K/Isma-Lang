#!/usr/bin/env bash
# Times Isma vs Rust (rustc -O) vs Python on the same programs.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p out
cargo build --release -q --manifest-path ../Cargo.toml
ISMA=../target/release/isma

best_of_3() {
    local best=""
    for _ in 1 2 3; do
        local s e t
        s=$(date +%s%N); "$@" > /dev/null; e=$(date +%s%N)
        t=$(( (e - s) / 1000000 ))
        if [[ -z "$best" || $t -lt $best ]]; then best=$t; fi
    done
    echo "$best"
}

printf "%-8s %10s %10s %10s\n" bench isma rust python
for b in fib sieve; do
    $ISMA build "$b.isma" -o "out/$b-isma"
    rustc -O "$b.rs" -o "out/$b-rust" 2>/dev/null
    expected=$("out/$b-rust")
    [[ "$("out/$b-isma")" == "$expected" ]] || { echo "MISMATCH in $b"; exit 1; }
    ti=$(best_of_3 "out/$b-isma")
    tr=$(best_of_3 "out/$b-rust")
    if command -v python3 > /dev/null; then tp="$(best_of_3 python3 "$b.py")ms"; else tp="n/a"; fi
    printf "%-8s %8sms %8sms %10s\n" "$b" "$ti" "$tr" "$tp"
done
