//! System tests for compiling Rust code with cargo.
//!
//! Any copyright is dedicated to the Public Domain.
//! http://creativecommons.org/publicdomain/zero/1.0/

use anyhow::{Context, Result};
use once_cell::sync::Lazy;

use assert_cmd::prelude::*;
use chrono::Local;
use predicates::prelude::*;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
#[macro_use]
extern crate log;

static SCCACHE_BIN: Lazy<PathBuf> = Lazy::new(|| assert_cmd::cargo::cargo_bin("sccache"));
static CARGO: Lazy<OsString> = Lazy::new(|| std::env::var_os("CARGO").unwrap());
static CRATE_DIR: Lazy<PathBuf> =
    Lazy::new(|| Path::new(file!()).parent().unwrap().join("test-crate"));

/// Used as a test setup fixture. The drop implementation cleans up after a _successfull_ test.
/// It is therefore important that the asserts use `Result` to return errors instead of panicking.
/// Otherwise the temporary directory will not be cleaned up and the started sccache server process
/// will not be stopped.
struct SccacheTest<'a> {
    /// Tempdir used for Sccache cache and cargo output. It is kept in the struct only to have the
    /// destructor run when SccacheTest goes out of scope, but is never used otherwise.
    #[allow(dead_code)]
    tempdir: tempfile::TempDir,
    env: Vec<(&'a str, std::ffi::OsString)>,
}

impl SccacheTest<'_> {
    fn new(additional_envs: Option<&[(&'static str, std::ffi::OsString)]>) -> Result<Self> {
        // Create a temp directory to use for the disk cache.
        let tempdir = tempfile::Builder::new()
            .prefix("sccache_test_rust_cargo")
            .tempdir()
            .context("Failed to create tempdir")?;
        let cache_dir = tempdir.path().join("cache");
        fs::create_dir(&cache_dir)?;
        let cargo_dir = tempdir.path().join("cargo");
        fs::create_dir(&cargo_dir)?;
        trace!("sccache --start-server");

        Command::new(SCCACHE_BIN.as_os_str())
            .arg("--start-server")
            .env("SCCACHE_DIR", &cache_dir)
            .assert()
            .try_success()
            .context("Failed to start sccache server")?;

        let mut env = vec![
            ("CARGO_TARGET_DIR", cargo_dir.as_os_str().to_owned()),
            ("RUSTC_WRAPPER", SCCACHE_BIN.as_os_str().to_owned()),
            // Explicitly disable incremental compilation because sccache is unable to cache it at
            // the time of writing.
            ("CARGO_INCREMENTAL", OsString::from("0")),
            ("SOME_ENV_VAR", OsString::from("SOME_VALUE")),
        ];

        if let Some(vec) = additional_envs {
            env.extend_from_slice(vec);
        }

        Ok(SccacheTest {
            tempdir,
            env: env.to_owned(),
        })
    }
}

impl Drop for SccacheTest<'_> {
    fn drop(&mut self) {
        stop_sccache().expect("Stopping Sccache server failed");
    }
}

fn stop_sccache() -> Result<()> {
    trace!("sccache --stop-server");

    Command::new(SCCACHE_BIN.as_os_str())
        .arg("--stop-server")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to stop sccache server")?;
    Ok(())
}

/// Test that building a simple Rust crate with cargo using sccache results in a cache hit
/// when built a second time and a cache miss, when the environment variable referenced via
/// env! is changed.
#[test]
#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn test_rust_cargo() -> Result<()> {
    drop(
        env_logger::Builder::new()
            .format(|f, record| {
                write!(
                    f,
                    "{} [{}] - {}",
                    Local::now().format("%Y-%m-%dT%H:%M:%S%.3f"),
                    record.level(),
                    record.args()
                )
            })
            .parse_env("RUST_LOG")
            .try_init(),
    );

    debug!("cargo: {:?}", CARGO);
    debug!("sccache: {:?}", SCCACHE_BIN);
    // Ensure there's no existing sccache server running.
    stop_sccache()?;

    test_rust_cargo_cmd("check", SccacheTest::new(None)?)
        .context("Sccache failed for `cargo check`")?;
    test_rust_cargo_cmd("build", SccacheTest::new(None)?)
        .context("Sccache failed for `cargo build`")?;

    #[cfg(feature = "unstable")]
    test_rust_cargo_cmd(
        "check",
        SccacheTest::new(&[("RUSTFLAGS", std::ffi::OsStr::new("-Zprofile"))]),
    )?;
    #[cfg(feature = "unstable")]
    test_rust_cargo_cmd(
        "build",
        SccacheTest::new(&[("RUSTFLAGS", std::ffi::OsStr::new("-Zprofile"))]),
    )?;

    Ok(())
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn test_rust_cargo_cmd(cmd: &str, test_info: SccacheTest) -> Result<()> {
    // `cargo clean` first, just to be sure there's no leftover build objects.
    Command::new(CARGO.as_os_str())
        .args(&["clean"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_success()?;
    // Now build the crate with cargo.
    Command::new(CARGO.as_os_str())
        .args(&[cmd, "--color=never"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8().not())?
        .try_success()?;
    // Clean it so we can build it again.
    Command::new(CARGO.as_os_str())
        .args(&["clean"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_success()?;
    Command::new(CARGO.as_os_str())
        .args(&[cmd, "--color=always"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8())?
        .try_success()?;

    // Now get the stats and ensure that we had a cache hit for the second build.
    // The test crate has one dependency (itoa) so there are two separate
    // compilations.
    trace!("sccache --show-stats");
    Command::new(SCCACHE_BIN.as_os_str())
        .args(&["--show-stats", "--stats-format=json"])
        .assert()
        .try_stdout(predicates::str::contains(r#""cache_hits":{"counts":{"Rust":2}}"#).from_utf8())?
        .try_success()?;

    drop(test_info);
    Ok(())
}
