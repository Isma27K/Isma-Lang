fn main() {
    let limit: usize = 10_000_000;
    let mut is_prime: Vec<bool> = Vec::new();
    for _ in 0..=limit {
        is_prime.push(true);
    }
    is_prime[0] = false;
    is_prime[1] = false;
    let mut i = 2;
    while i * i <= limit {
        if is_prime[i] {
            let mut j = i * i;
            while j <= limit {
                is_prime[j] = false;
                j += i;
            }
        }
        i += 1;
    }
    let count = is_prime.iter().filter(|&&p| p).count();
    println!("{count}");
}
