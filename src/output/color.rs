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
    /// Compute the policy from the runner's `--color=` setting, the
    /// original-stdout TTY status, and the environment snapshot.
    /// Precedence: `FORCE_COLOR` (if set, colour on regardless of
    /// everything else) > explicit `--color=always|never` >
    /// `NO_COLOR` (if set, colour off) > `--color=auto` + TTY check.
    #[must_use]
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

    /// A colour-off policy — convenience for non-terminal paths.
    #[must_use]
    pub const fn off() -> Self {
        Self { use_color: false }
    }

    /// Whether colour escapes should be emitted.
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.use_color
    }

    /// Wrap `s` in red (`\x1b[31m`). Returns `s` unchanged when
    /// colour is off.
    #[must_use]
    pub fn red(self, s: &str) -> String {
        self.wrap(s, "31")
    }

    /// Wrap `s` in green (`\x1b[32m`). Returns `s` unchanged when
    /// colour is off.
    #[must_use]
    pub fn green(self, s: &str) -> String {
        self.wrap(s, "32")
    }

    /// Wrap `s` in yellow (`\x1b[33m`). Returns `s` unchanged when
    /// colour is off.
    #[must_use]
    pub fn yellow(self, s: &str) -> String {
        self.wrap(s, "33")
    }

    /// Wrap `s` in dim / faint (`\x1b[2m`). Returns `s` unchanged when
    /// colour is off.
    #[must_use]
    pub fn dim(self, s: &str) -> String {
        self.wrap(s, "2")
    }

    /// Wrap `s` in bold (`\x1b[1m`). Returns `s` unchanged when
    /// colour is off.
    #[must_use]
    pub fn bold(self, s: &str) -> String {
        self.wrap(s, "1")
    }

    fn wrap(self, s: &str, code: &str) -> String {
        if self.use_color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }
}
