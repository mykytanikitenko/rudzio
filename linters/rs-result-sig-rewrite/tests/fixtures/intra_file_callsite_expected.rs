fn helper(x: i32) -> anyhow::Result<i32> {
    Ok(x)
}

fn caller() -> anyhow::Result<i32> {
    Ok(helper(1)?)
}
