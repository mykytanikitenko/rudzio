//! Conversion helpers between tokio's task errors and [`JoinError`].

use tokio::task::JoinError as TokioJoinError;

use crate::runtime::JoinError;

/// Convert a tokio [`TokioJoinError`] into the framework's [`JoinError`].
#[inline]
pub(crate) fn tokio_join_error_to_join_error(err: TokioJoinError) -> JoinError {
    if err.is_panic() {
        JoinError::panicked(err)
    } else {
        JoinError::cancelled(err)
    }
}
