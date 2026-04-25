#[cfg(test)]
mod tests {
    #[test]
    fn addition_works() {
        assert_eq!(1 + 1, 2);
    }

    #[bench]
    fn addition_bench(b: &mut test::Bencher) {
        b.iter(|| 1 + 1);
    }
}
