//! Reproducer: src/ unit tests need `[lib] harness = false` in
//! Cargo.toml plus `#[cfg(test)] #[rudzio::main] fn main() {}` at
//! the bottom of lib.rs, otherwise libtest remains in control and
//! rudzio's converted tests never run.

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_correctly() {
        assert_eq!(add(1, 2), 3);
    }
}
