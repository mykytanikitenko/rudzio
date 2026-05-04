//! Deterministic, seed-driven Fisher–Yates shuffle.
//!
//! Used by the runner when `--shuffle` / `--shuffle-seed=<N>` is on:
//! every `(runtime, suite)` group's test list is permuted in place
//! before dispatch using the same seed, so a re-run with the same seed
//! reproduces the order. The PRNG is SplitMix64 — small, has good
//! statistical properties for this use case, and avoids pulling a
//! `rand` dependency into the runtime crate.

/// Permute `items` in place using `seed` to drive a Fisher–Yates pass.
///
/// Same seed → same permutation, regardless of platform or build,
/// since the PRNG is implemented inline and the loop walks indices in
/// a fixed order. Empty and single-element slices are no-ops.
#[inline]
pub fn seeded_shuffle<T>(items: &mut [T], seed: u64) {
    let len = items.len();
    if len < 2 {
        return;
    }
    let mut state = seed;
    let mut idx = len - 1;
    while idx > 0 {
        state = next_splitmix64(state);
        // Map the 64-bit PRNG output to `[0, idx]`. `idx + 1` cannot
        // overflow because `idx < usize::MAX` (the slice length fits
        // in `usize`).
        let modulus = (idx + 1) as u64;
        let pick = (state % modulus) as usize;
        items.swap(idx, pick);
        idx -= 1;
    }
}

/// One step of the SplitMix64 PRNG. Seeded with the previous state
/// (or the user's starting seed); returns the next pseudo-random u64.
#[inline]
fn next_splitmix64(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
