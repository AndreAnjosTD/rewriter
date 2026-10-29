//! Library surface over the recipe engine.
//!
//! The binary is the product; this target exists so integration tests can drive
//! the snippet machinery in process instead of spawning the binary for every
//! case.

pub mod engine;
pub mod recipes;
