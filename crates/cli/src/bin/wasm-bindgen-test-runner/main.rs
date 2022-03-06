//! A "wrapper binary" used to execute wasm files as tests
//!
//! This binary is intended to be used as a "test runner" for wasm binaries,
//! being compatible with `cargo test` for the wasm target. It will
//! automatically execute `wasm-bindgen` (or the equivalent thereof) and then
//! execute either Node.js over the tests or start a server which a browser can
//! be used to run against to execute tests. In a browser mode if `CI` is in the
//! environment then it'll also attempt headless testing, spawning the server in
//! the background and then using the WebDriver protocol to execute tests.
//!
//! For more documentation about this see the `wasm-bindgen-test` crate README
//! and source code.

use anyhow::{anyhow, bail, Context};
use std::env;
use std::fs;
use std::path::PathBuf;
use wasm_bindgen_cli_support::Bindgen;

// no need for jemalloc bloat in this binary (and we don't need speed)
#[global_allocator]
static ALLOC: std::alloc::System = std::alloc::System;

mod browser;
mod deno;
mod headless;
mod node;
mod server;
mod shell;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum TestMode {
    Node,
    Deno,
    Browser,
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let mut args = env::args_os().skip(1);
    let shell = shell::Shell::new();

    // Currently no flags are supported, and assume there's only one argument
    // which is the wasm file to test. This'll want to improve over time!
    let wasm_file_to_test = match args.next() {
        Some(file) => PathBuf::from(file),
        None => bail!("must have a file to test as first argument"),
    };

    // Assume a cargo-like directory layout and generate output at
    // `target/wasm32-unknown-unknown/wbg-tmp/...`
    let tmpdir = wasm_file_to_test
        .parent() // chop off file name
        .and_then(|p| p.parent()) // chop off `deps`
        .and_then(|p| p.parent()) // chop off `debug`
        .map(|p| p.join("wbg-tmp"))
        .ok_or_else(|| anyhow!("file to test doesn't follow the expected Cargo conventions"))?;

    // Make sure there's no stale state from before
    drop(fs::remove_dir_all(&tmpdir));
    fs::create_dir(&tmpdir).context("creating temporary directory")?;

    let module = "wasm-bindgen-test";

    // Collect all tests that the test harness is supposed to run. We assume
    // that any exported function with the prefix `__wbg_test` is a test we need
    // to execute.
    let wasm = fs::read(&wasm_file_to_test).context("failed to read wasm file")?;
    let mut wasm =
        walrus::Module::from_buffer(&wasm).context("failed to deserialize wasm module")?;

    let tests = wasm
        .exports
        .iter()
        .filter(|exp| exp.name.starts_with("__wbgt_"))
        .map(|exp| exp.name.to_string())
        .collect::<Vec<_>>();

    // Right now there's a bug where if no tests are present then the
    // `wasm-bindgen-test` runtime support isn't linked in, so just bail out
    // early saying everything is ok.
    if tests.len() == 0 {
        println!("no tests to run!");
        return Ok(());
    }

    // Figure out if this tests is supposed to execute in node.js or a browser.
    // That's done on a per-test-binary basis with the
    // `wasm_bindgen_test_configure` macro, which emits a custom section for us
    // to read later on.

    let custom_section = wasm.customs.remove_raw("__wasm_bindgen_test_unstable");
    let test_mode = match custom_section {
        Some(section) if section.data.contains(&0x01) => TestMode::Browser,
        Some(_) => bail!("invalid __wasm_bingen_test_unstable value"),
        None if std::env::var("WASM_BINDGEN_USE_DENO").is_ok() => TestMode::Deno,
        None => TestMode::Node,
    };

    // Gracefully handle requests to execute only node or only web tests.
    let node = test_mode == TestMode::Node;

    if env::var_os("WASM_BINDGEN_TEST_ONLY_NODE").is_some() {
        if !node {
            println!(
                "this test suite is only configured to run in a browser, \
                 but we're only testing node.js tests so skipping"
            );
            return Ok(());
        }
    }
    if env::var_os("WASM_BINDGEN_TEST_ONLY_WEB").is_some() {
        if node {
            println!(
                "\
This test suite is only configured to run in node.js, but we're only running
browser tests so skipping. If you'd like to run the tests in a browser
include this in your crate when testing:

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

You'll likely want to put that in a `#[cfg(test)]` module or at the top of an
integration test.\
"
            );
            return Ok(());
        }
    }

    // Make the generated bindings available for the tests to execute against.
    shell.status("Executing bindgen...");
    let mut b = Bindgen::new();
    match test_mode {
        TestMode::Node => b.nodejs(true)?,
        TestMode::Deno => b.deno(true)?,
        TestMode::Browser => b.web(true)?,
    };

    let debug = env::var("WASM_BINDGEN_DEBUG").is_ok();
    b.debug(debug)
        .input_module(module, wasm)
        .keep_debug(false)
        .emit_start(false)
        .generate(&tmpdir)
        .context("executing `wasm-bindgen` over the wasm file")?;
    shell.clear();

    let args: Vec<_> = args.collect();

    match test_mode {
        TestMode::Node => node::execute(module, &tmpdir, &args, &tests)?,
        TestMode::Deno => deno::execute(module, &tmpdir, &args, &tests)?,
        TestMode::Browser => browser::execute(shell, module, &tmpdir, &args, tests)?,
    }
    Ok(())
}
