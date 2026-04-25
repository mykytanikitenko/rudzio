#[cfg(test)]
mod tests {
    #[test]
    #[ignore]
    fn bare_ignore() {}

    #[test]
    #[ignore = "reason as name-value"]
    fn named_ignore() {}

    #[test]
    #[ignore("reason as list-literal")]
    fn list_literal_ignore() {}

    #[test]
    fn not_ignored() {}
}
