#[cfg_attr(any(test, rudzio_test), derive(Debug))]
pub struct Thing {
    pub n: u32,
}
pub fn identity(n: u32) -> u32 {
    n
}
