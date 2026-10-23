//! Deterministic, seed-driven Fisher–Yates shuffle.
//!
//! Used by the runner when `--shuffle` / `--shuffle-seed=<N>` is on:
//! every `(runtime, suite)` group's test list is permuted in place
//! before dispatch using the same seed, so a re-run with the same seed
//! reproduces the order. The PRNG is `SplitMix64` — small, has good
//! statistical properties for this use case, and avoids pulling a
//! `rand` dependency into the runtime crate.

/// One step of the `SplitMix64` PRNG. Seeded with the previous state
/// (or the user's starting seed); returns the next pseudo-random u64.
#[inline]
const fn next_splitmix64(state: u64) -> u64 {
    let mut next = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    next = (next ^ (next >> 30_u32)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    next = (next ^ (next >> 27_u32)).wrapping_mul(0x94D0_49BB_1331_11EB);
    next ^ (next >> 31_u32)
}

/// Permute `items` in place using `seed` to drive a Fisher–Yates pass.
///
/// Same seed → same permutation, regardless of platform or build,
/// since the PRNG is implemented inline and the loop walks indices in
/// a fixed order. Empty and single-element slices are no-ops.
///
/// # Errors
///
/// Currently infallible — always returns `Ok(())`. The `Result`
/// return is part of the workspace-wide Result-cleanup (PLAN.md M-2):
/// the signature is converted ahead of the body so callers adopt
/// `?`-propagation now. The internal `unwrap_or` fallbacks (flagged
/// by the inline `TODO(rs-cleanup)` markers) are the future failure
/// points to convert into propagated errors.
// TODO(rs-cleanup): permute_with_seed is currently total; once the
// internal try_from fallbacks become real propagated errors, this
// `# Errors` section must describe them.
#[inline]
pub fn permute_with_seed<T>(items: &mut [T], seed: u64) -> anyhow::Result<()> {
    let len = items.len();
    let Some(mut idx) = len.checked_sub(1_usize) else {
        return Ok(());
    };
    if idx == 0_usize {
        return Ok(());
    }
    let mut state = seed;
    while idx > 0_usize {
        state = next_splitmix64(state);
        // Map the 64-bit PRNG output to `[0, idx]`. `idx + 1` cannot
        // overflow because `idx < usize::MAX` (the slice length fits
        // in `usize`).
        // TODO(rs-cleanup): silencing at line 40 — review and replace with Result-propagating equivalent
        let modulus = u64::try_from(idx.saturating_add(1_usize)).unwrap_or(u64::MAX);
        let bucket = state.rem_euclid(modulus);
        // TODO(rs-cleanup): silencing at line 42 — review and replace with Result-propagating equivalent
        let pick = usize::try_from(bucket).unwrap_or(0_usize);
        items.swap(idx, pick);
        // TODO(rs-cleanup): masking_arith at line 44 — review and replace with Result-propagating equivalent
        idx = idx.saturating_sub(1_usize);
    }
    Ok(())
}
