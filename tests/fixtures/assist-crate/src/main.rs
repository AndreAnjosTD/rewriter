//! A crate with a local variable that is worth inlining.
//!
//! rust-analyzer's "Inline variable" assist rewrites `sum` into the expression
//! that produced it, which is a small, checkable change.

fn total(items: &[u32]) -> u32 {
    let sum = items.iter().sum::<u32>();
    sum * 2
}

fn main() {
    println!("{}", total(&[1, 2, 3]));
}
