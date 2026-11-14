class Vec2:
    __slots__ = ("x", "y")

    def __init__(self, x, y):
        self.x = x
        self.y = y

    def add(self, o):
        return Vec2(self.x + o.x, self.y + o.y)


def main():
    pos = Vec2(0.0, 0.0)
    vel = Vec2(1.0, 0.5)
    gravity = Vec2(0.0, -0.001)
    for _ in range(5_000_000):
        vel = vel.add(gravity)
        pos = pos.add(vel)
    print(int(pos.x))
    print(int(pos.y))


main()
