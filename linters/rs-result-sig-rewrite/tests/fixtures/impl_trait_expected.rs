fn iter() -> anyhow::Result<impl Iterator<Item = u32>> {
    // FIXME(rs-cleanup): tail expr returns impl Trait; verify Ok-wrap compiles
    Ok(std::iter::empty())
}
