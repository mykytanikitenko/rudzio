fn first<T: Clone>(item: T) -> T where T: std::fmt::Debug {
    item.clone()
}
