// Binary trees: build and walk 3 million short-lived tree nodes.
struct Tree {
    left: Option<Box<Tree>>,
    right: Option<Box<Tree>>,
}

fn make(depth: i64) -> Box<Tree> {
    if depth == 0 {
        return Box::new(Tree { left: None, right: None });
    }
    Box::new(Tree { left: Some(make(depth - 1)), right: Some(make(depth - 1)) })
}

fn check(t: &Option<Box<Tree>>) -> i64 {
    match t {
        None => 0,
        Some(n) => 1 + check(&n.left) + check(&n.right),
    }
}

fn main() {
    let max_depth = 14;
    let long_lived = Some(make(max_depth));
    let mut total = 0;
    let mut depth = 4;
    while depth <= max_depth {
        let iterations = 1i64 << (max_depth - depth + 4);
        for _ in 0..iterations {
            total += check(&Some(make(depth)));
        }
        depth += 2;
    }
    println!("{}", total);
    println!("{}", check(&long_lived));
}
