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
/// Propagates an `anyhow` error if the index arithmetic or numeric
/// narrowing that maps the PRNG output onto `[0, idx]` overflows.
/// Those steps are infallible for a well-formed slice (the length fits
/// in `usize`, and `bucket` is strictly bounded by it), but they are
/// kept fallible so a corrupted length surfaces loudly instead of
/// silently truncating the permutation.
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
        let modulus = u64::try_from(
            idx.checked_add(1_usize)
                .ok_or_else(|| anyhow::anyhow!("shuffle: index overflow"))?,
        )
        .map_err(|_ignored| anyhow::anyhow!("shuffle: index overflows u64"))?;
        let bucket = state.rem_euclid(modulus);
        let pick = usize::try_from(bucket)
            .map_err(|_ignored| anyhow::anyhow!("shuffle: pick overflows usize"))?;
        items.swap(idx, pick);
        idx = idx
            .checked_sub(1_usize)
            .ok_or_else(|| anyhow::anyhow!("shuffle: index underflow"))?;
    }
    Ok(())
}
