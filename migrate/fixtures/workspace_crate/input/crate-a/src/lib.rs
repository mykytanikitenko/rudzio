pub fn a_answer() -> i32 {
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_works() {
        assert_eq!(a_answer(), 10);
    }
}
