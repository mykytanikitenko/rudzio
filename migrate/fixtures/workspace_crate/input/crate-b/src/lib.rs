pub fn b_answer() -> i32 {
    20
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b_works() {
        assert_eq!(b_answer(), 20);
    }
}
