struct C;

impl Iterator for C {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        Some(1)
    }
}
