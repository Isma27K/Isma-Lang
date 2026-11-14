# Binary trees: build and walk 3 million short-lived tree nodes.
def make(depth):
    if depth == 0:
        return (None, None)
    return (make(depth - 1), make(depth - 1))


def check(t):
    if t is None:
        return 0
    return 1 + check(t[0]) + check(t[1])


def main():
    max_depth = 14
    long_lived = make(max_depth)
    total = 0
    depth = 4
    while depth <= max_depth:
        for _ in range(2 ** (max_depth - depth + 4)):
            total += check(make(depth))
        depth += 2
    print(total)
    print(check(long_lived))


main()
