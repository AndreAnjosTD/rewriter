//! A crate built to be ambiguous on purpose.
//!
//! Two types define a method with the same name, so a text-level rename cannot
//! tell them apart and only a type-aware one can. One method is a trait
//! implementation, which the analyzer must refuse to rename.

#[derive(Debug)]
struct Custom;

impl Clone for Custom {
    fn clone(&self) -> Self {
        Custom
    }
}

struct Bag;

impl Bag {
    /// The target of the ambiguous rename.
    fn len(&self) -> usize {
        0
    }

    fn capacity(&self) -> usize {
        16
    }
}

struct Server;

impl Server {
    fn start(&self) -> &'static str {
        "started"
    }

    fn stop(&self) -> &'static str {
        "stopped"
    }
}

fn main() {
    let text = String::from("hello");
    let copy = text.clone();
    let custom = Custom;
    let custom_copy = custom.clone();
    let bag = Bag;
    let n = bag.len();
    let room = bag.capacity();
    let numbers: Vec<u32> = vec![1, 2, 3];
    let count = numbers.len();
    let server = Server;
    let first = server.start();
    let again = server.start();
    let third = server.start();
    let second = server.stop();
    println!("{copy} {custom_copy:?} {n} {room} {count} {first} {again} {third} {second}");
}
