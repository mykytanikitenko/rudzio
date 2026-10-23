//! Behavioural reproducer + proactive regression guard for Bug #19:
//! bin source mounted into the bridge crate (Bug #17 fix at
//! 820d257) broke `crate::*` path resolution inside the bin's own
//! source tree.
//!
//! # Symptom
//!
//! Bug #17's fix mounted each `[[bin]]` source tree into the
//! member's bridge crate via
//!
//! ```rust,ignore
//! #[path = "<abs>/src/bin/<role>/main.rs"]
//! mod __rudzio_bin_<bin_name>;
//! ```
//!
//! Test markers (`#[rudzio::test]`) inside the bin's submodule
//! tree correctly reached the bridge's `linkme` slice, fixing the
//! original Bug #16 silent-skip problem.
//!
//! But the bin source itself was authored on the assumption that
//! `crate::` is rooted at the BIN crate. Every `crate::api::foo`,
//! `crate::config::Bar`, etc. inside the bin tree resolved against
//! the BRIDGE crate (where `api`/`config`/etc. were NOT direct
//! children of the crate root — they sat one level deeper under
//! `crate::__rudzio_bin_<bin_name>::…`), producing hundreds of
//! `E0432: unresolved import 'crate::api'` errors and a bridge
//! that never compiled.
//!
//! # Fix shape
//!
//! Each bin that would have been mounted via Phase 0a's
//! `collect_unmounted_bin_entries` instead becomes its own bridge
//! crate: `<aggregator>/members/<member>__bin__<bin>/` with
//! `[lib] path = "<abs>/src/bin/<role>/main.rs"`. The bin's
//! `main.rs` is now the lib crate's root, so `crate::api` resolves
//! to the per-bin bridge's `api` mod — exactly the resolution the
//! bin code was written against.
//!
//! Test markers inside the bin source register via `linkme` into
//! the per-bin bridge crate; the aggregator `extern crate`s the
//! per-bin bridge so linkme's cross-crate aggregation pulls those
//! `TEST_TOKENS` into the aggregator's final binary.
//!
//! # Proactive coverage
//!
//! The test set exercises multiple shapes the per-bin bridge fix
//! must support — not just the originally-reported `crate::api`
//! shape:
//!
//! 1. `bin_using_crate_paths_compiles_and_runs` — user's exact
//!    repro: `crate::api::helper()` inside the bin's own
//!    `mod tests` block.
//! 2. `multi_bin_each_with_independent_crate_root` — two bins per
//!    member; each must get its own crate root so their
//!    `crate::*` paths don't collide.
//! 3. `deeply_nested_bin_crate_paths_resolve` — the file-v3
//!    pattern: `crate::api::handler::common::Foo` at depth 3+.
//! 4. `bin_using_member_lib_via_use_resolves` — bin code does
//!    `use member_lib::Foo;` referencing the member's lib; per-bin
//!    bridge's dep mirroring must make this work.
//! 5. `standalone_bin_with_rudzio_main_remains_standalone` —
//!    `fixtures/src/bin/*.rs` pattern: a bin that contains its own
//!    `#[rudzio::main]` self-aggregates. Per-bin bridge MUST NOT
//!    be created for these (would double-register their tests).
//! 6. `default_bin_with_shared_mod_via_lib_runs_test_once` —
//!    Phase 0a Test 3 invariant preserved: a `mod common;`
//!    declaration shared between `src/lib.rs` and `src/main.rs`
//!    runs its tests exactly once (no double-mount via per-bin
//!    bridge).
//!
//! # Phase B migration
//!
//! Each `customize_*` helper starts from the suite-provided
//! `ctx.consumer_workspace()` (template = workspace + minimal
//! consumer) and mutates the consumer member to match the
//! fixture's target shape. The template's workspace `Cargo.toml`
//! provides the rudzio path-dep + anyhow workspace deps; per-test
//! `consumer/Cargo.toml` overwrites reference them via
//! `{ workspace = true }`.

use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;

/// **Test 1 fixture** — user's exact Bug #19 repro:
///
/// ```text
/// <workspace>/
/// └── consumer/
///     ├── Cargo.toml                   ([lib] + [[bin]] consumer-server)
///     └── src/
///         ├── lib.rs                   (trivial — pulls bin into rudzio_test)
///         └── bin/
///             └── server/
///                 ├── main.rs          (mod api;)
///                 └── api/
///                     └── mod.rs       (pub fn helper(); + test using crate::api::helper)
/// ```
///
/// The test inside `api/mod.rs` references `crate::api::helper()`
/// — pre-fix this fails to compile because `crate::` resolves to
/// the BRIDGE, not the bin.
fn customize_test_1_crate_api_helper(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let api_dir = consumer.join("src").join("bin").join("server").join("api");
    fs::create_dir_all(&api_dir)?;

    fs::write(
        consumer.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[bin]]\n\
         name = \"consumer-server\"\n\
         path = \"src/bin/server/main.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;

    // Trivial lib — exists to make `[lib]` declarable.
    fs::write(consumer.join("src").join("lib.rs"), "// stub lib\n")?;

    fs::write(
        consumer
            .join("src")
            .join("bin")
            .join("server")
            .join("main.rs"),
        "mod api;\n\
         \n\
         fn main() {\n\
         \x20\x20\x20\x20println!(\"consumer-server stub\");\n\
         }\n",
    )?;

    fs::write(
        api_dir.join("mod.rs"),
        "pub fn helper() -> i32 { 42 }\n\
         \n\
         // rudzio_test marker\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn helper_returns_42(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20// `crate::api` must resolve to the bin's `api` mod,\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20// not the bridge crate's root -- this is Bug #19.\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(crate::api::helper() == 42);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;

    Ok(())
}

/// **Test 2 fixture** — multi-bin: two `[[bin]]` entries each
/// with its own `crate::*` paths. Each must get its own per-bin
/// bridge so `crate::api_one::value()` in bin one and
/// `crate::api_two::value()` in bin two resolve INDEPENDENTLY,
/// without cross-contamination.
fn customize_test_2_multi_bin_crate_isolation(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let bin_one_dir = consumer.join("src").join("bin").join("one");
    let bin_two_dir = consumer.join("src").join("bin").join("two");
    fs::create_dir_all(&bin_one_dir)?;
    fs::create_dir_all(&bin_two_dir)?;

    fs::write(
        consumer.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[bin]]\n\
         name = \"consumer-one\"\n\
         path = \"src/bin/one/main.rs\"\n\
         \n\
         [[bin]]\n\
         name = \"consumer-two\"\n\
         path = \"src/bin/two/main.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;

    fs::write(consumer.join("src").join("lib.rs"), "// stub lib\n")?;

    // Bin one declares `mod api_one;` and tests against `crate::api_one`.
    fs::write(
        bin_one_dir.join("main.rs"),
        "mod api_one;\n\
         \n\
         fn main() { println!(\"one\"); }\n\
         \n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn bin_one_sees_its_own_api(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(crate::api_one::VALUE == 1);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;
    fs::write(
        bin_one_dir.join("api_one.rs"),
        "pub const VALUE: i32 = 1;\n",
    )?;

    // Bin two declares `mod api_two;` and tests against `crate::api_two`.
    fs::write(
        bin_two_dir.join("main.rs"),
        "mod api_two;\n\
         \n\
         fn main() { println!(\"two\"); }\n\
         \n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn bin_two_sees_its_own_api(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(crate::api_two::VALUE == 2);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;
    fs::write(
        bin_two_dir.join("api_two.rs"),
        "pub const VALUE: i32 = 2;\n",
    )?;

    Ok(())
}

/// **Test 3 fixture** — file-v3-shape: `crate::api::handler::common`
/// at depth 3+ in the bin tree:
///
/// ```text
/// consumer/src/bin/server/
/// ├── main.rs                  (mod api;)
/// ├── api/
/// │   ├── mod.rs               (pub mod handler;)
/// │   ├── handler/
/// │   │   ├── mod.rs           (pub mod common; pub mod create;)
/// │   │   ├── common.rs        (pub type Resp = i32;)
/// │   │   └── create.rs        (uses crate::api::handler::common::Resp + test)
/// ```
fn customize_test_3_deep_crate_paths(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let server_dir = consumer.join("src").join("bin").join("server");
    let handler_dir = server_dir.join("api").join("handler");
    fs::create_dir_all(&handler_dir)?;

    fs::write(
        consumer.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[bin]]\n\
         name = \"consumer-server\"\n\
         path = \"src/bin/server/main.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;
    fs::write(consumer.join("src").join("lib.rs"), "// stub lib\n")?;

    fs::write(
        server_dir.join("main.rs"),
        "mod api;\n\nfn main() { println!(\"deep\"); }\n",
    )?;
    fs::write(server_dir.join("api").join("mod.rs"), "pub mod handler;\n")?;
    fs::write(
        handler_dir.join("mod.rs"),
        "pub mod common;\npub mod create;\n",
    )?;
    fs::write(handler_dir.join("common.rs"), "pub type Resp = i32;\n")?;

    fs::write(
        handler_dir.join("create.rs"),
        "use crate::api::handler::common::Resp;\n\
         \n\
         pub fn make() -> Resp { 7 }\n\
         \n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn deep_crate_path_resolves(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20let resp: crate::api::handler::common::Resp = crate::api::handler::create::make();\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(resp == 7);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;

    Ok(())
}

/// **Test 4 fixture** — bin uses member lib via `use consumer::*`:
///
/// ```text
/// consumer/
/// ├── src/lib.rs              (pub fn lib_helper() -> i32 { 17 })
/// └── src/bin/server/
///     └── main.rs             (use consumer::lib_helper; + test asserting == 17)
/// ```
fn customize_test_4_bin_uses_member_lib(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let server_dir = consumer.join("src").join("bin").join("server");
    fs::create_dir_all(&server_dir)?;

    fs::write(
        consumer.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[bin]]\n\
         name = \"consumer-server\"\n\
         path = \"src/bin/server/main.rs\"\n\
         \n\
         [dependencies]\n\
         consumer = { path = \".\" }\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;

    fs::write(
        consumer.join("src").join("lib.rs"),
        "pub fn lib_helper() -> i32 { 17 }\n",
    )?;

    fs::write(
        server_dir.join("main.rs"),
        "use consumer::lib_helper;\n\
         \n\
         fn main() { println!(\"{}\", lib_helper()); }\n\
         \n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn bin_can_use_member_lib(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(consumer::lib_helper() == 17);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;

    Ok(())
}

/// **Test 5 fixture** — standalone bin with its own `#[rudzio::main]`:
///
/// ```text
/// consumer/src/bin/runner.rs   (flat bin; #[rudzio::main] + 1 #[rudzio::test])
/// ```
///
/// Mirrors the in-tree `fixtures/src/bin/*.rs` pattern. The bin
/// self-aggregates via its own `#[rudzio::main]`, so the
/// per-bin-bridge machinery must NOT wrap it — that would
/// double-register every test in the bin.
///
/// The structural test for the regression: `cargo rudzio test`
/// reports the bin's test exactly ONCE (it self-aggregates as a
/// standalone runner, not via the aggregator's main).
fn customize_test_5_standalone_bin(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let bin_dir = consumer.join("src").join("bin");
    fs::create_dir_all(&bin_dir)?;

    fs::write(
        consumer.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[bin]]\n\
         name = \"runner\"\n\
         path = \"src/bin/runner.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;
    fs::write(consumer.join("src").join("lib.rs"), "// stub lib\n")?;

    fs::write(
        bin_dir.join("runner.rs"),
        "#[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn standalone_runner_test(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n\
         \n\
         #[rudzio::main]\n\
         fn main() {}\n",
    )?;

    Ok(())
}

/// **Test 6 fixture** — default `src/main.rs` + `src/lib.rs`
/// sharing `mod common;` (Phase 0a Test 3 invariant carried
/// forward):
///
/// ```text
/// consumer/
/// ├── src/
/// │   ├── lib.rs               (mod common;)
/// │   ├── main.rs              (mod common; fn main)
/// │   └── common.rs            (1 #[rudzio::test] — common_shared_test)
/// ```
///
/// Under the per-bin bridge fix, `src/main.rs` (`file_name` ==
/// "main.rs", parent == `lib_dir`) STILL must NOT trigger per-bin
/// bridge creation — `is_directory_style_bin` is false for this
/// layout, so no per-bin bridge is built, and `common_shared_test`
/// runs exactly once via the lib mount only.
fn customize_test_6_default_bin_shared_mod(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let src_dir = consumer.join("src");
    fs::create_dir_all(&src_dir)?;

    fs::write(
        consumer.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[bin]]\n\
         name = \"consumer-cli\"\n\
         path = \"src/main.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;

    fs::write(src_dir.join("lib.rs"), "mod common;\n")?;
    fs::write(
        src_dir.join("main.rs"),
        "mod common;\n\
         \n\
         fn main() { println!(\"cli\"); }\n",
    )?;
    fs::write(
        src_dir.join("common.rs"),
        "#[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn common_shared_test(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;

    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        Command, IsolatedCargoTest, Output, Path, Stdio, customize_test_1_crate_api_helper,
        customize_test_2_multi_bin_crate_isolation, customize_test_3_deep_crate_paths,
        customize_test_4_bin_uses_member_lib, customize_test_5_standalone_bin,
        customize_test_6_default_bin_shared_mod, io,
    };
    use anyhow::Context as _;

    /// **Test 4**: bin code uses the member's lib via
    /// `use consumer::lib_helper`. The per-bin bridge's
    /// dependency table must include the member's bridge as
    /// `consumer = { path = "..", package = "consumer_rudzio_bridge" }`
    /// so the resolution works.
    #[rudzio::test]
    async fn bin_using_member_lib_via_use_resolves(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        customize_test_4_bin_uses_member_lib(workspace)
            .with_context(|| "test4 step1: customize_test_4_bin_uses_member_lib")?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())
            .with_context(|| "test4 step2: run_cargo_rudzio_test")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        anyhow::ensure!(
            output.status.success(),
            "Run failed (member-lib use scenario): status={:?}\nstdout={stdout}\nstderr={stderr}",
            output.status.code(),
        );
        anyhow::ensure!(
            stdout.contains("bin_can_use_member_lib"),
            "`bin_can_use_member_lib` test did not appear in stdout. \
             The per-bin bridge's deps table must mirror the member's deps \
             (including the path-rename to the member's bridge crate) so the \
             bin's `use consumer::lib_helper` resolves.\nstdout={stdout}\nstderr={stderr}",
        );
        anyhow::ensure!(
            stdout.contains(" 1 passed;"),
            "Expected exactly 1 passed (1 bin test). stdout={stdout}",
        );
        Ok(())
    }

    /// **Test 1 — user's Bug #19 repro**: a `#[rudzio::test]`
    /// inside a `[[bin]]` source file references `crate::api::*`,
    /// where `api` is `src/bin/<role>/api/mod.rs`. Pre-fix the
    /// bridge compile fails with `E0432 unresolved import
    /// 'crate::api'` because `crate::` resolves to the BRIDGE
    /// crate, not the bin. Post-fix the bin compiles as its own
    /// per-bin bridge LIB with `crate::` rooted at the bin's
    /// `main.rs`, so `crate::api::helper` resolves cleanly.
    #[rudzio::test]
    async fn bin_using_crate_paths_compiles_and_runs(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        customize_test_1_crate_api_helper(workspace)
            .with_context(|| "test1 step1: customize_test_1_crate_api_helper")?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())
            .with_context(|| "test1 step2: run_cargo_rudzio_test")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        anyhow::ensure!(
            output.status.success(),
            "Bug #19 reproduces: `cargo rudzio test` exited {:?}; expected success.\n\
             Pre-fix this fails with E0432 unresolved import 'crate::api' \
             because the bin source is mounted as a sub-module of the bridge, \
             not as its own lib crate.\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status.code(),
        );
        anyhow::ensure!(
            stdout.contains("helper_returns_42"),
            "`helper_returns_42` test did not appear in stdout -- bin source \
             with `crate::*` paths failed to compile or register.\nstdout={stdout}",
        );
        anyhow::ensure!(
            stdout.contains(" 1 passed;"),
            "Expected exactly 1 passed (the bin test). stdout={stdout}",
        );
        Ok(())
    }

    /// **Test 3 — file-v3 deep-nesting pattern**: bin source at
    /// `src/bin/server/api/handler/create.rs` uses
    /// `crate::api::handler::common::Resp` and
    /// `crate::api::handler::create::make`. Per-bin bridge must
    /// support arbitrarily-deep `crate::*` paths so that real
    /// production bin trees (~hundreds of `crate::` refs across
    /// many handler files) compile under `cargo rudzio test`.
    #[rudzio::test]
    async fn deeply_nested_bin_crate_paths_resolve(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        customize_test_3_deep_crate_paths(workspace)
            .with_context(|| "test3 step1: customize_test_3_deep_crate_paths")?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())
            .with_context(|| "test3 step2: run_cargo_rudzio_test")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        anyhow::ensure!(
            output.status.success(),
            "Run failed (deep crate paths scenario): status={:?}\nstdout={stdout}\nstderr={stderr}",
            output.status.code(),
        );
        anyhow::ensure!(
            stdout.contains("deep_crate_path_resolves"),
            "`deep_crate_path_resolves` test did not appear in stdout. \
             Per-bin bridge must support depth-3+ `crate::*` paths.\nstdout={stdout}",
        );
        anyhow::ensure!(
            stdout.contains(" 1 passed;"),
            "Expected exactly 1 passed. stdout={stdout}",
        );
        Ok(())
    }

    /// **Test 6**: `src/main.rs` + `src/lib.rs` sharing `mod common;`.
    /// Phase 0a's `is_directory_style_bin` heuristic guards against
    /// double-mount: parent of `src/main.rs` equals `lib_dir`, so
    /// the bin is classified flat-style → NOT a per-bin bridge
    /// candidate. `common_shared_test` runs exactly ONCE via the
    /// lib mount. Regression guard: if the new per-bin bridge
    /// logic forgets this guard, the test would register twice.
    #[rudzio::test]
    async fn default_bin_with_shared_mod_via_lib_runs_test_once(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        customize_test_6_default_bin_shared_mod(workspace)
            .with_context(|| "test6 step1: customize_test_6_default_bin_shared_mod")?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())
            .with_context(|| "test6 step2: run_cargo_rudzio_test")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        anyhow::ensure!(
            output.status.success(),
            "Run failed (default-bin shared-mod scenario): status={:?}\nstdout={stdout}\nstderr={stderr}",
            output.status.code(),
        );

        let occurrences = stdout.matches("common_shared_test").count();
        anyhow::ensure!(
            occurrences == 1,
            "`common_shared_test` ran {occurrences} times; expected exactly 1. \
             If 2: the lib_dir guard regressed (a per-bin bridge was synthesised \
             for `src/main.rs` whose parent equals lib_dir). \
             If 0: the lib mount path is broken.\nstdout={stdout}",
        );
        anyhow::ensure!(
            stdout.contains(" 1 passed;"),
            "Expected exactly 1 passed (1 shared-mod test). stdout={stdout}",
        );
        Ok(())
    }

    /// **Test 2**: two `[[bin]]` entries per member, each with its
    /// own `crate::*` paths. Each MUST get its own per-bin bridge
    /// so the crate roots are independent. Cross-contamination
    /// (e.g., bin-one's `crate::api_one` resolving via bin-two's
    /// crate root) would mean per-bin bridges are wrongly shared.
    #[rudzio::test]
    async fn multi_bin_each_with_independent_crate_root(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        customize_test_2_multi_bin_crate_isolation(workspace)
            .with_context(|| "test2 step1: customize_test_2_multi_bin_crate_isolation")?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())
            .with_context(|| "test2 step2: run_cargo_rudzio_test")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        anyhow::ensure!(
            output.status.success(),
            "Run failed (multi-bin scenario): status={:?}\nstdout={stdout}\nstderr={stderr}",
            output.status.code(),
        );
        anyhow::ensure!(
            stdout.contains("bin_one_sees_its_own_api"),
            "bin_one_sees_its_own_api missing -- per-bin bridge for bin one didn't compile/register.\nstdout={stdout}",
        );
        anyhow::ensure!(
            stdout.contains("bin_two_sees_its_own_api"),
            "bin_two_sees_its_own_api missing -- per-bin bridge for bin two didn't compile/register.\nstdout={stdout}",
        );
        anyhow::ensure!(
            stdout.contains(" 2 passed;"),
            "Expected exactly 2 passed (1 per bin). stdout={stdout}",
        );
        Ok(())
    }

    /// Spawn `cargo-rudzio test` against the fixture.
    fn run_cargo_rudzio_test(
        root: &Path,
        cargo_home: &Path,
        cargo_target_dir: &Path,
    ) -> io::Result<Output> {
        let cargo_rudzio_bin: &str = env!("CARGO_BIN_EXE_cargo-rudzio");
        Command::new(cargo_rudzio_bin)
            .arg("test")
            .current_dir(root)
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_TARGET_DIR", cargo_target_dir)
            .env("CARGO_NET_OFFLINE", "true")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
    }

    /// **Test 5**: a `[[bin]]` whose source file contains
    /// `#[rudzio::main]` is a self-aggregating standalone runner.
    /// The per-bin bridge machinery MUST NOT wrap it — doing so
    /// would compile the bin once as its own standalone runner
    /// AND once inside the per-bin bridge, double-registering
    /// every test.
    ///
    /// Regression guard: this preserves the Bug #16 step 2.5 /
    /// `collect_unmounted_bin_entries` behaviour where standalone
    /// bins are skipped.
    #[rudzio::test]
    async fn standalone_bin_with_rudzio_main_remains_standalone(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        customize_test_5_standalone_bin(workspace)
            .with_context(|| "test5 step1: customize_test_5_standalone_bin")?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())
            .with_context(|| "test5 step2: run_cargo_rudzio_test")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        anyhow::ensure!(
            output.status.success(),
            "Run failed (standalone-bin scenario): status={:?}\nstdout={stdout}\nstderr={stderr}",
            output.status.code(),
        );

        // The standalone bin's test runs via its OWN binary (the
        // `#[rudzio::main]` makes it self-aggregating), not via
        // the cargo-rudzio aggregator. Its tests therefore don't
        // appear in the aggregator's stdout. The aggregator
        // should report 0 tests from this consumer (no lib
        // tests, bin is self-aggregating and skipped).
        let occurrences = stdout.matches("standalone_runner_test").count();
        anyhow::ensure!(
            occurrences == 0,
            "`standalone_runner_test` appeared {occurrences} times in the \
             aggregator's stdout -- expected 0. A standalone bin (one with \
             `#[rudzio::main]`) self-aggregates as its own runner and must \
             NOT also be wrapped by a per-bin bridge; doing so would \
             double-register every test.\nstdout={stdout}",
        );
        Ok(())
    }
}
