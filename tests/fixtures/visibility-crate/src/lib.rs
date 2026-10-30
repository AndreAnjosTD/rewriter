//! Fixture for the visibility primitive.
//!
//! Each test copies this crate and changes one declaration. The functions are
//! shaped to catch an insertion that lands after a modifier: `pub async fn` is
//! the order Rust accepts, and the plain, `async` and already-restricted
//! declarations each exercise a different insertion or removal path.

pub struct Service {
    value: u32,
}

impl Service {
    pub fn value(&self) -> u32 {
        self.value
    }

    fn hidden(&self) -> u32 {
        0
    }
}

fn helper() -> u32 {
    1
}

pub fn exported() -> u32 {
    helper()
}

pub(crate) fn restricted() -> u32 {
    2
}

async fn work() {}

pub fn exercise() -> u32 {
    let service = Service { value: 1 };
    service.value() + service.hidden() + helper() + exported() + restricted()
}
