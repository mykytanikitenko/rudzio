/// Adds one.
/// # Errors
///
/// Returns `Err` if an underlying operation fails. TODO(rs-cleanup): describe the failure conditions.
pub fn public_add(x: i32) -> anyhow::Result<i32> {
    Ok(x)
}

fn private_add(x: i32) -> anyhow::Result<i32> {
    Ok(x)
}
