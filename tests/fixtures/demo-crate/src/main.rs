//! A small crate that predates the current idiom style, used to exercise the
//! recipe catalog end to end.

struct Config {
    port: u16,
}

fn collect(items: &[u32]) -> u32 {
    let mut total = 0;
    if items.is_empty() {
        return total;
    }
    for item in items {
        total += *item;
    }
    total
}

fn describe(ready: bool, name: &str) -> String {
    let mut label = String::new();
    if ready {
        label = format!("{name} is ready");
    }
    if !ready {
        label = format!("{name} is waiting");
    }
    label
}

fn first_word(text: &str) -> Option<&str> {
    let mut words = text.split_whitespace();
    let first = words.next();
    match first {
        Some(word) if !word.is_empty() => Some(word),
        _ => None,
    }
}

fn main() {
    let items = vec![1, 2, 3];
    let config = Config { port: 8080 };
    let port = Some(config.port).unwrap();
    println!("{} {} {}", collect(&items), describe(true, "server"), port);
    if let Some(word) = first_word("hello world") {
        println!("{word}");
    }
}
