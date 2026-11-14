#[derive(Clone, Copy)]
struct Vec2 {
    x: f64,
    y: f64,
}

impl Vec2 {
    fn add(self, o: Vec2) -> Vec2 {
        Vec2 { x: self.x + o.x, y: self.y + o.y }
    }
}

fn main() {
    let mut pos = Vec2 { x: 0.0, y: 0.0 };
    let mut vel = Vec2 { x: 1.0, y: 0.5 };
    let gravity = Vec2 { x: 0.0, y: -0.001 };
    for _ in 0..5_000_000 {
        vel = vel.add(gravity);
        pos = pos.add(vel);
    }
    println!("{}", pos.x as i64);
    println!("{}", pos.y as i64);
}
