limit = 10_000_000
is_prime = []
for i in range(limit + 1):
    is_prime.append(True)
is_prime[0] = False
is_prime[1] = False
i = 2
while i * i <= limit:
    if is_prime[i]:
        j = i * i
        while j <= limit:
            is_prime[j] = False
            j += i
    i += 1
print(sum(1 for p in is_prime if p))
