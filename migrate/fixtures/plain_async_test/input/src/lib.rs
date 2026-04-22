pub fn identity<T>(v: T) -> T {
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    async fn already_async() {
        let n = identity(7);
        assert_eq!(n, 7);
    }
}
