#!/usr/bin/env bash
# Times Tarn vs Rust (rustc -O) vs Python on the same programs.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p out
cargo build --release -q --manifest-path ../Cargo.toml
TARN=../target/release/tarn

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

printf "%-8s %10s %10s %10s\n" bench tarn rust python
results=()
for b in fib sieve strings particles trees maps; do
    $TARN build "$b.tr" -o "out/$b-tarn"
    rustc -O "$b.rs" -o "out/$b-rust" 2>/dev/null
    expected=$("out/$b-rust")
    [[ "$("out/$b-tarn")" == "$expected" ]] || { echo "MISMATCH in $b"; exit 1; }
    ti=$(best_of_3 "out/$b-tarn")
    tr=$(best_of_3 "out/$b-rust")
    if command -v python3 > /dev/null; then tp="$(best_of_3 python3 "$b.py")ms"; else tp="n/a"; fi
    printf "%-8s %8sms %8sms %10s\n" "$b" "$ti" "$tr" "$tp"
    results+=("\"$b\": {\"tarn\": $ti, \"rust\": $tr, \"python\": ${tp%ms}}")
done
# Machine-readable results (used by docs/media/generate.py for the README chart).
{
    echo "{"
    echo "  \"date\": \"$(date -u +%Y-%m-%d)\","
    echo "  \"cpu\": \"$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | sed 's/^ *//' || uname -m)\","
    (IFS=,; echo "  ${results[*]}" | sed 's/},"/},\n  "/g')
    echo "}"
} > results.json
echo "wrote bench/results.json"
