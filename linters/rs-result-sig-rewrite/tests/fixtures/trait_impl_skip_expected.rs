struct X;

impl PartialEq for X {
    fn eq(&self, other: &Self) -> bool {
        true
    }
}
