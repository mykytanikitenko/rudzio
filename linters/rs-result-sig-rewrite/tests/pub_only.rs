//! `--pub-only` mode: only bare-`pub` fns are rewritten; private and
//! `pub(crate)` fns are left untouched (so the converted signatures
//! cannot trip `clippy::unnecessary_wraps`, which only exempts
//! exported functions). See `.opencode/issues.md` 2026-05-29.

use rs_result_sig_rewrite::transform_with_options;

const INPUT: &str = "\
pub fn public_total() -> u32 {
    1
}

fn private_total() -> u32 {
    2
}

pub(crate) fn crate_total() -> u32 {
    3
}
";

#[test]
fn pub_only_transforms_only_bare_pub() {
    let outcome = transform_with_options(INPUT, true).expect("transform must succeed");
    // Only `public_total` converts.
    assert_eq!(outcome.transformed, 1, "only the bare-pub fn should convert");
    assert!(
        outcome.new_source.contains("pub fn public_total() -> anyhow::Result<u32>"),
        "public_total must be converted; got:\n{src}",
        src = outcome.new_source,
    );
    assert!(
        outcome.new_source.contains("fn private_total() -> u32"),
        "private_total must be left untouched; got:\n{src}",
        src = outcome.new_source,
    );
    assert!(
        outcome.new_source.contains("pub(crate) fn crate_total() -> u32"),
        "pub(crate) fn must be left untouched; got:\n{src}",
        src = outcome.new_source,
    );
}

#[test]
fn default_mode_transforms_all_three() {
    let outcome = transform_with_options(INPUT, false).expect("transform must succeed");
    assert_eq!(
        outcome.transformed, 3,
        "default (pub_only=false) converts every non-skipped fn",
    );
}
