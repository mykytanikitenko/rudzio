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
/// All arithmetic is performed in `u128` integer space through
/// `checked_*` so the workspace `arithmetic_side_effects` /
/// `integer_division` lints stay green; the divisors are non-zero
/// literals, but the checked form is kept so a future non-literal
/// divisor would surface as an error instead of a silent wrap.
#[inline]
pub fn fmt_duration(dur: Duration) -> anyhow::Result<String> {
    let nanos = dur.as_nanos();
    let (unit, divisor) = if nanos >= 1_000_000_000_u128 {
        ("s", 10_000_000_u128)
    } else if nanos >= 1_000_000_u128 {
        ("ms", 10_000_u128)
    } else if nanos >= 1_000_u128 {
        ("\u{b5}s", 10_u128)
    } else {
        return Ok(format!("{nanos}.00ns"));
    };
    let centi = nanos
        .checked_div(divisor)
        .ok_or_else(|| anyhow::anyhow!("duration division by {divisor}"))?;
    let int_part = centi
        .checked_div(100_u128)
        .ok_or_else(|| anyhow::anyhow!("duration division by 100"))?;
    let frac_part = centi
        .checked_rem(100_u128)
        .ok_or_else(|| anyhow::anyhow!("duration remainder by 100"))?;
    Ok(format!("{int_part}.{frac_part:02}{unit}"))
}
