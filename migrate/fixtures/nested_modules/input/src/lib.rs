pub fn product(a: i32, b: i32) -> i32 {
    a * b
}

#[cfg(test)]
mod outer {
    use super::*;

    mod inner {
        use super::*;

        #[test]
        fn inner_multiplies() {
            assert_eq!(product(3, 4), 12);
        }
    }

    #[test]
    fn outer_multiplies() {
        assert_eq!(product(5, 6), 30);
    }
}
