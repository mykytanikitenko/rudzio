fn first<T: Clone>(item: T) -> anyhow::Result<T> where T: std::fmt::Debug {
    Ok(item.clone())
}
