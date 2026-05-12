//! Asserts rudzio captures stdout/stderr per test, attributes each
//! stream to the originating test in the output, and surfaces panic
//! messages to stderr.
//!
//! Three tests print distinct markers; the panicking one additionally
//! aborts via `panic!` with a unique message. The integration test
//! scrapes the binary's combined output and checks:
//!   - every per-test marker appears exactly where expected;
//!   - the panic message appears in the failing test's block, not
//!     alpha's or beta's.

use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::unnecessary_wraps,
        reason = "this fixture asserts rudzio attributes captured stdout/stderr to the originating test; the integration test greps the combined binary output for these per-test markers, so println!/eprintln! are the deliberate channels and the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn alpha(_ctx: &Test) -> anyhow::Result<()> {
        println!("alpha_stdout_line_1");
        println!("alpha_stdout_line_2");
        println!("alpha_stdout_line_3");
        eprintln!("alpha_stderr_line_1");
        eprintln!("alpha_stderr_line_2");
        eprintln!("alpha_stderr_line_3");
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::unnecessary_wraps,
        reason = "this fixture asserts rudzio attributes captured stdout/stderr to the originating test; the integration test greps the combined binary output for these per-test markers, so println!/eprintln! are the deliberate channels and the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn beta(_ctx: &Test) -> anyhow::Result<()> {
        println!("beta_stdout_line_1");
        println!("beta_stdout_line_2");
        println!("beta_stdout_line_3");
        eprintln!("beta_stderr_line_1");
        eprintln!("beta_stderr_line_2");
        eprintln!("beta_stderr_line_3");
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::panic,
        clippy::print_stdout,
        clippy::print_stderr,
        reason = "this fixture asserts rudzio attributes the panic message to the failing test (not the surrounding alpha/beta); the panic! is the test scenario being exercised and println!/eprintln! emit the markers the integration test greps"
    )]
    fn gamma_panics(_ctx: &Test) -> anyhow::Result<()> {
        println!("gamma_stdout_line_1");
        println!("gamma_stdout_line_2");
        println!("gamma_stdout_line_3");
        eprintln!("gamma_stderr_line_1");
        eprintln!("gamma_stderr_line_2");
        eprintln!("gamma_stderr_line_3");
        panic!("gamma_panic_message_line_1\ngamma_panic_message_line_2")
    }
}

#[rudzio::main]
fn main() {}
