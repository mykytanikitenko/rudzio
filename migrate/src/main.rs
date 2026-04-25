//! Binary entry point. All orchestration lives in
//! `rudzio_migrate::run::entry` so the library surface and the
//! integration tests can drive the same flow.

use std::process::ExitCode;

fn main() -> ExitCode {
    rudzio_migrate::run::entry()
}
