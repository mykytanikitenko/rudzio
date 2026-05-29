fn pick(b: bool) -> anyhow::Result<i32> {
    if b {
        return Ok(42);
    }
    Ok(7)
}
