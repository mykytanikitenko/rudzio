#[cfg_attr(any(test, rudzio_test), derive(Debug, Clone))]
pub struct Widget {
    pub size: u32,
}
pub struct Gadget {
    pub name: String,
}
#[cfg_attr(any(test, rudzio_test), derive(PartialEq))]
impl Gadget {
    pub fn new(name: String) -> Self {
        Self { name }
    }
}
#[cfg(any(test, rudzio_test))]
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::tokio::Multithread::new,
        suite = ::rudzio::common::context::Suite,
        test = ::rudzio::common::context::Test,
    ),
    ]
)]
mod tests {
    use super::*;
    /* pre-migration (rudzio-migrate):
    #[test]
    fn widget_has_size() {
        let w = Widget { size: 42 };
        assert_eq!(w.size, 42);
    }
    */
    #[::rudzio::test]
    async fn widget_has_size() {
        let w = Widget { size: 42 };
        assert_eq!(w.size, 42);
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
