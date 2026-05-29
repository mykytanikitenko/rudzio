fn maybe_exit(b: bool) -> anyhow::Result<()> {
    if b {
        return Ok(());
    }
    println!("ok");
    Ok(())
}
