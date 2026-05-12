//! Binary entry point. All orchestration lives in
//! `rudzio_migrate::run::entry` so the library surface and the
//! integration tests can drive the same flow.

use std::process::ExitCode;

use rudzio_migrate::run;

fn main() -> ExitCode {
    run::entry()
}
