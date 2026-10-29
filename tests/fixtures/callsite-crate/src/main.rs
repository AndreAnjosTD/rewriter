//! A crate with multiple call sites for the same functions.
//!
//! The point is that changing an argument list has to move the declaration and
//! every call together. `scope` exists to be removed, which is why it is not read
//! by the bodies.

#[allow(unused_variables)]
fn record(scope: &str, message: &str) -> String {
    format!("recorded {message}")
}

struct Report;

impl Report {
    #[allow(unused_variables)]
    fn emit(&self, scope: &str, message: &str) -> String {
        format!("emit {message}")
    }
}

fn main() {
    let a = record("disk", "mounted");
    let b = record("net", "connected");
    let c = record("cpu", "hot");
    let report = Report;
    let d = report.emit("disk", "full");
    let e = report.emit("net", "slow");
    println!("{a} {b} {c} {d} {e}");
}
