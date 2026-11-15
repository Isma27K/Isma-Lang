# Counting with hash maps: 2 million updates with Int keys, then 2 million
# with String keys (word frequencies).
counts = {}
x = 12345
for i in range(2_000_000):
    x = (x * 1103515245 + 12345) % 2147483648
    k = x % 500_000
    counts[k] = counts.get(k, 0) + 1
total = 0
for k, c in counts.items():
    total += k % 7 * c
print(len(counts))
print(total)

words = [f"w{i * 7919 % 20_000}" for i in range(200_000)]
freq = {}
for _ in range(10):
    for w in words:
        freq[w] = freq.get(w, 0) + 1
print(len(freq))
print(freq["w0"])
