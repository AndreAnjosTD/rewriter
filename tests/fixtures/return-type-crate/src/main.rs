//! Fixture for the return-type and resolved-rename primitives.
//!
//! Each test copies this crate and changes one declaration, so everything a test
//! does not use still has to compile. The typed and untyped call sites are
//! deliberate: one accepts a widened return type, the other does not.

pub struct Version {
    major: u8,
}

impl Version {
    pub fn major(&self) -> u8 {
        self.major
    }

    pub fn describe(&self) -> String {
        format!("v{}", self.major)
    }
}

/// The call site is untyped, so either `usize` or `u64` compiles here.
pub fn retries() -> usize {
    3
}

/// This call site is typed, so widening the return type must fail the gate.
pub fn limit() -> usize {
    30
}

/// No annotation: the body diverges, so `()` is what it returns today.
pub fn forever() {
    loop {}
}

pub const MAX: u32 = 100;

pub struct Config;

impl Config {
    pub const MAX: u32 = 10;
}

pub struct Reading {
    pub value: u32,
}

pub struct Sample {
    pub value: u64,
}

pub fn main() {
    let version = Version { major: 1 };
    let major: u8 = version.major();
    let label = version.describe();
    let attempts = retries();
    let ceiling: usize = limit();
    let configured = MAX;
    let nested = Config::MAX;
    let reading = Reading { value: 1 };
    let sample = Sample { value: 2 };
    // Kept out of `println!` on purpose: rust-analyzer's rename does not reach
    // tokens inside a macro invocation, and this fixture proves the use outside
    // one is updated while the same-named field on `Sample` is not.
    let reading_value = reading.value;
    let sample_value = sample.value;
    println!(
        "{major} {label} {attempts} {ceiling} {configured} {nested} {reading_value} {sample_value}"
    );
    if major == 0 {
        forever();
    }
}
