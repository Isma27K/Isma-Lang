def main():
    s = ""
    total = 0
    for i in range(2_000_000):
        part = f"{i},"
        total += len(part)
        s += part
    print(total)
    print(len(s))


main()
