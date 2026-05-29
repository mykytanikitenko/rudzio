fn parse(s: &str) -> anyhow::Result<i32> {
    let x: i32 = s.parse::<i32>()?;
    Ok(x)
}
