#[cfg(test)]
mod tests {
    #[test]
    #[should_panic]
    fn panics_as_expected() {
        panic!("expected");
    }

    #[test]
    fn ordinary_ok() {
        assert!(true);
    }
}
