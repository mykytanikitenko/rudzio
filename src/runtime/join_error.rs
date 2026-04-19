use std::error::Error;
use std::fmt;

/// Errors that can occur when awaiting a spawned task.
#[derive(Debug)]
#[non_exhaustive]
pub enum JoinError {
    /// The task was cancelled.
    Cancelled {
        /// The underlying error if available.
        source: Option<Box<dyn Error + Send + Sync + 'static>>,
    },
    /// The task panicked.
    Panicked {
        /// The underlying error if available.
        source: Option<Box<dyn Error + Send + Sync + 'static>>,
    },
}

impl JoinError {
    /// Create a new cancelled error with the given source.
    #[inline]
    #[must_use]
    pub fn cancelled<E>(source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::Cancelled {
            source: Some(Box::new(source)),
        }
    }

    /// Create a new cancelled error without a source.
    #[inline]
    #[must_use]
    pub const fn cancelled_simple() -> Self {
        Self::Cancelled { source: None }
    }

    /// Create a new panicked error with the given source.
    #[inline]
    #[must_use]
    pub fn panicked<E>(source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::Panicked {
            source: Some(Box::new(source)),
        }
    }

    /// Create a new panicked error without a source.
    #[inline]
    #[must_use]
    pub const fn panicked_simple() -> Self {
        Self::Panicked { source: None }
    }
}

impl fmt::Display for JoinError {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Panicked { .. } => write!(f, "task panicked"),
            Self::Cancelled { .. } => write!(f, "task was cancelled"),
        }
    }
}

impl Error for JoinError {
    #[inline]
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        let source = match self {
            Self::Panicked { source } | Self::Cancelled { source } => source,
        };
        source.as_ref().map(|boxed| {
            let err: &(dyn Error + 'static) = boxed.as_ref();
            err
        })
    }
}
