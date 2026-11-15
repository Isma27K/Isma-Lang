// Counting with hash maps: 2 million updates with Int keys, then 2 million
// with String keys (word frequencies). Rust's standard HashMap.
use std::collections::HashMap;

fn main() {
    let mut counts: HashMap<i64, i64> = HashMap::new();
    let mut x: i64 = 12345;
    for _ in 0..2_000_000 {
        x = (x * 1103515245 + 12345) % 2147483648;
        let k = x % 500_000;
        *counts.entry(k).or_insert(0) += 1;
    }
    let mut total = 0;
    for (k, c) in &counts {
        total += k % 7 * c;
    }
    println!("{}", counts.len());
    println!("{total}");

    let words: Vec<String> = (0..200_000).map(|i| format!("w{}", i * 7919 % 20_000)).collect();
    let mut freq: HashMap<String, i64> = HashMap::new();
    for _ in 0..10 {
        for w in &words {
            match freq.get_mut(w.as_str()) {
                Some(c) => *c += 1,
                None => {
                    freq.insert(w.clone(), 1);
                }
            }
        }
    }
    println!("{}", freq.len());
    println!("{}", freq["w0"]);
}
