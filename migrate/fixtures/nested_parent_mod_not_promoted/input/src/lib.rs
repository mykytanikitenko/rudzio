//! Reproducer: a non-cfg(test) parent module hosts a cfg(test)
//! `mod tests`. Only the inner `mod tests` should be wrapped with
//! `#[rudzio::suite]` — wrapping `outer` would fail the macro's
//! "at least one #[rudzio::test]" assertion in non-test builds
//! and would drag the lib's normal-code module through rudzio's
//! codegen for no reason.

pub fn root() -> i32 { 1 }

pub mod outer {
    pub fn inner() -> i32 { super::root() + 1 }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn nested() {
            assert_eq!(inner(), 2);
        }
    }
}
