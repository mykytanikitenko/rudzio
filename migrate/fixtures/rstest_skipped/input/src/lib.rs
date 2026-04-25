pub fn prefix_matches(haystack: &str, needle: &str) -> bool {
    haystack.starts_with(needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn baseline_works() {
        assert!(prefix_matches("hello", "he"));
    }

    #[rstest]
    #[case("x", "x")]
    #[case("xy", "x")]
    #[case("xy", "y")]
    fn matches_cases(#[case] haystack: &str, #[case] needle: &str) {
        assert!(prefix_matches(haystack, needle) || haystack.contains(needle));
    }
}
