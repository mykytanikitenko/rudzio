//! Distribution-summary fields of a [`crate::bench::ProgressSnapshot`],
//! bundled so the snapshot constructor takes one struct instead of six
//! positional args (sidesteps `clippy::too_many_arguments`).

use std::time::Duration;

use crate::bench::progress_snapshot::HISTOGRAM_BUCKETS;

/// Distribution-summary fields of a [`crate::bench::ProgressSnapshot`].
///
/// Bundled so [`crate::bench::ProgressSnapshot::new`] takes one struct
/// instead of six positional args, sidestepping
/// `clippy::too_many_arguments`.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct DistSummary {
    /// See [`crate::bench::ProgressSnapshot::cov_permille`].
    pub cov_permille: Option<u16>,
    /// See [`crate::bench::ProgressSnapshot::histogram`].
    pub histogram: [u32; HISTOGRAM_BUCKETS],
    /// See [`crate::bench::ProgressSnapshot::max`].
    pub max: Duration,
    /// See [`crate::bench::ProgressSnapshot::min`].
    pub min: Duration,
    /// See [`crate::bench::ProgressSnapshot::p50`].
    pub p50: Duration,
    /// See [`crate::bench::ProgressSnapshot::p95`].
    pub p95: Duration,
}

impl DistSummary {
    /// Pack the distribution-summary fields.
    #[inline]
    #[must_use]
    pub const fn new(
        cov_permille: Option<u16>,
        histogram: [u32; HISTOGRAM_BUCKETS],
        max: Duration,
        min: Duration,
        p50: Duration,
        p95: Duration,
    ) -> Self {
        Self {
            cov_permille,
            histogram,
            max,
            min,
            p50,
            p95,
        }
    }
}
