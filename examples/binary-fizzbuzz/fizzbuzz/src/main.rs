//! A correct reference solution for the `binary-fizzbuzz` example
//! assignment: prints FizzBuzz for `1..=n`, where `n` is the sole CLI argument.

use std::env;

fn main() {
    let n: u32 = env::args()
        .nth(1)
        .expect("usage: fizzbuzz <n>")
        .parse()
        .expect("<n> must be a non-negative integer");

    for i in 1..=n {
        let line = match (i % 3, i % 5) {
            (0, 0) => "FizzBuzz".to_string(),
            (0, _) => "Fizz".to_string(),
            (_, 0) => "Buzz".to_string(),
            _ => i.to_string(),
        };
        println!("{line}");
    }
}
