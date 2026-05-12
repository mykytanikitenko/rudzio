//! Display-side helpers for `Duration`.
//!
//! [`fmt_duration`] is the integer-arithmetic counterpart to the
//! `{d:.2?}` Debug specifier on `Duration`: same `xx.yy<unit>` shape,
//! produced through `u128` math so call sites don't trip
//! `clippy::use_debug` and the formatter cannot panic.

use std::time::Duration;

/// Format `dur` as `xx.yy<unit>` with two decimal places.
///
/// Picks the largest unit (`s` / `ms` / `µs` / `ns`) whose integer
/// value is non-zero. The output mirrors the `{d:.2?}` Debug rendering
/// so call sites that previously used Debug formatting can swap to this
/// helper without changing the rendered string.
///
/// All arithmetic is performed in `u128` integer space — there is no
/// floating-point conversion and no fallible operation, so the helper
/// is total over `Duration`.
#[must_use]
#[inline]
pub fn fmt_duration(dur: Duration) -> String {
    let nanos = dur.as_nanos();
    if nanos >= 1_000_000_000_u128 {
        let centi = nanos.checked_div(10_000_000_u128).unwrap_or(0_u128);
        let int_part = centi.checked_div(100_u128).unwrap_or(0_u128);
        let frac_part = centi.checked_rem(100_u128).unwrap_or(0_u128);
        format!("{int_part}.{frac_part:02}s")
    } else if nanos >= 1_000_000_u128 {
        let centi = nanos.checked_div(10_000_u128).unwrap_or(0_u128);
        let int_part = centi.checked_div(100_u128).unwrap_or(0_u128);
        let frac_part = centi.checked_rem(100_u128).unwrap_or(0_u128);
        format!("{int_part}.{frac_part:02}ms")
    } else if nanos >= 1_000_u128 {
        let centi = nanos.checked_div(10_u128).unwrap_or(0_u128);
        let int_part = centi.checked_div(100_u128).unwrap_or(0_u128);
        let frac_part = centi.checked_rem(100_u128).unwrap_or(0_u128);
        format!("{int_part}.{frac_part:02}\u{b5}s")
    } else {
        format!("{nanos}.00ns")
    }
}
