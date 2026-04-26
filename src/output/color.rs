//! ANSI colour policy resolution for the runner's output.
//!
//! Resolved once at startup (before [`crate::output::pipe`] swaps the
//! FDs — otherwise `is_terminal()` on FD 1 would report the pipe, not
//! the real terminal) from [`ColorMode`], the saved-original stdout's
//! TTY status, and the `NO_COLOR` / `FORCE_COLOR` conventions.

use std::collections::BTreeMap;

use crate::config::ColorMode;

/// Whether ANSI colour escapes should be emitted by the renderer.
///
/// Obtain one via [`ColorPolicy::resolve`] at runner startup; pass it
/// around instead of re-querying the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorPolicy {
    use_color: bool,
}

impl ColorPolicy {
    /// Wrap `text` in bold (`\x1b[1m`). Returns `text` unchanged when
    /// colour is off.
    #[must_use]
    #[inline]
    pub fn bold(self, text: &str) -> String {
        self.wrap(text, "1")
    }

    /// Wrap `text` in dim / faint (`\x1b[2m`). Returns `text` unchanged when
    /// colour is off.
    #[must_use]
    #[inline]
    pub fn dim(self, text: &str) -> String {
        self.wrap(text, "2")
    }

    /// Whether colour escapes should be emitted.
    #[must_use]
    #[inline]
    pub const fn enabled(self) -> bool {
        self.use_color
    }

    /// Wrap `text` in green (`\x1b[32m`). Returns `text` unchanged when
    /// colour is off.
    #[must_use]
    #[inline]
    pub fn green(self, text: &str) -> String {
        self.wrap(text, "32")
    }

    /// A colour-off policy — convenience for non-terminal paths.
    #[must_use]
    #[inline]
    pub const fn off() -> Self {
        Self { use_color: false }
    }

    /// Wrap `text` in red (`\x1b[31m`). Returns `text` unchanged when
    /// colour is off.
    #[must_use]
    #[inline]
    pub fn red(self, text: &str) -> String {
        self.wrap(text, "31")
    }

    /// Compute the policy from the runner's `--color=` setting, the
    /// original-stdout TTY status, and the environment snapshot.
    /// Precedence: `FORCE_COLOR` (if set, colour on regardless of
    /// everything else) > explicit `--color=always|never` >
    /// `NO_COLOR` (if set, colour off) > `--color=auto` + TTY check.
    #[must_use]
    #[inline]
    pub fn resolve(mode: ColorMode, stdout_is_tty: bool, env: &BTreeMap<String, String>) -> Self {
        if env.contains_key("FORCE_COLOR") {
            return Self { use_color: true };
        }
        let use_color = match mode {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => stdout_is_tty && !env.contains_key("NO_COLOR"),
        };
        Self { use_color }
    }

    fn wrap(self, text: &str, code: &str) -> String {
        if self.use_color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    /// Wrap `text` in yellow (`\x1b[33m`). Returns `text` unchanged when
    /// colour is off.
    #[must_use]
    #[inline]
    pub fn yellow(self, text: &str) -> String {
        self.wrap(text, "33")
    }
}
