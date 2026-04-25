pub fn parse_u8(s: &str) -> Result<u8, std::num::ParseIntError> {
    s.parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The body is a block expression `{ ... ; Ok(()) }` — the outer
    // `Stmt::Expr` is a block, not an `Ok(...)` call. An
    // `ends_with_ok` heuristic that only peeks at the top-level tail
    // expression would miss the inner `Ok` and double-append its
    // own. Capturing the ORIGINAL return type keeps this clean.
    #[test]
    fn parses_via_block_body() -> Result<(), std::num::ParseIntError> {
        {
            let n = parse_u8("42")?;
            assert_eq!(n, 42);
            Ok(())
        }
    }
}
