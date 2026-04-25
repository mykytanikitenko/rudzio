#[cfg_attr(test, derive(Debug, Clone))]
pub struct Widget {
    pub size: u32,
}

pub struct Gadget {
    pub name: String,
}

#[cfg_attr(test, derive(PartialEq))]
impl Gadget {
    pub fn new(name: String) -> Self {
        Self { name }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_has_size() {
        let w = Widget { size: 42 };
        assert_eq!(w.size, 42);
    }
}
