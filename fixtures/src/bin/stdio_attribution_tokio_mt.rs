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

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
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
