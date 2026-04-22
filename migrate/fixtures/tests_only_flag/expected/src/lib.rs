pub fn ident<T>(v: T) -> T {
    v
}

// `--tests-only` should leave this `#[cfg(test)] mod` alone — the
// expected snapshot keeps it as stock cargo-style, untouched by the
// migration.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn src_test_untouched() {
        assert_eq!(ident(3), 3);
    }
}
