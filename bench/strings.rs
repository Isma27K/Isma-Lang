fn main() {
    let mut s = String::new();
    let mut total = 0;
    for i in 0..2_000_000 {
        let part = format!("{i},");
        total += part.len();
        s.push_str(&part);
    }
    println!("{total}");
    println!("{}", s.len());
}
