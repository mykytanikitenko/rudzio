fn helper(x: i32) -> anyhow::Result<i32> {
    Ok(x)
}

fn caller() -> anyhow::Result<i32> {
    // FIXME(rs-cleanup): callee returns Result, add ?
    Ok(helper(1))
}
