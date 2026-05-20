//! Integration tests for `.zst`/`.zstd` input support. Gated behind the
//! `zeekstd` feature so the suite still runs with `--no-default-features`.

#![cfg(feature = "zeekstd")]

mod common;

use common::{big_fixture, fixture_path, riplog_bin, run};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Run `zeekstd compress` against `input` and cache the resulting archive
/// alongside the plain fixture. Panics with a clear message if `zeekstd` is
/// not on `PATH`.
fn compress_seekztd(input: &Path, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("riplog-it");
    fs::create_dir_all(&dir).unwrap();
    let out = dir.join(name);
    let _ = fs::remove_file(&out);
    let st = Command::new("zeekstd")
        .args(["compress", "--quiet", "-o"])
        .arg(&out)
        .arg(input)
        .status()
        .expect(
            "zeekstd CLI not found on PATH; install with `cargo install zeekstd_cli` to run \
             seekztd tests",
        );
    assert!(st.success(), "zeekstd compress failed for {input:?}");
    out
}

fn seekztd_fixture() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| compress_seekztd(fixture_path(), "fixture.log.seekztd"))
        .as_path()
}

fn big_seekztd_fixture() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| compress_seekztd(big_fixture(), "big-fixture.log.seekztd"))
        .as_path()
}

/// Compress the fixture with vanilla streaming-zstd (no seek table) using
/// the `zstd` crate. Cached for the test process.
fn streaming_zst_fixture() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let src = fs::read(fixture_path()).unwrap();
        let dir = std::env::temp_dir().join("riplog-it");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture-streaming.zst");
        let f = fs::File::create(&path).unwrap();
        let mut enc = zstd::stream::write::Encoder::new(f, 3).unwrap();
        enc.write_all(&src).unwrap();
        enc.finish().unwrap();
        path
    })
    .as_path()
}

#[test]
fn count_no_filter_seekztd() {
    let path = seekztd_fixture().to_str().unwrap();
    let out = run(&["--count", path]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "200");
}

#[test]
fn group_by_level_seekztd() {
    let plain = run(&[
        "--count",
        "--group-by=level",
        fixture_path().to_str().unwrap(),
    ]);
    let zstd = run(&[
        "--count",
        "--group-by=level",
        seekztd_fixture().to_str().unwrap(),
    ]);
    assert_eq!(
        plain.stdout, zstd.stdout,
        "group-by output must match plain fixture"
    );
}

#[test]
fn from_to_filters_seekztd() {
    let path = seekztd_fixture().to_str().unwrap();
    let out = run(&[
        "--from=2026-04-24T18:00:05Z",
        "--to=2026-04-24T18:00:05.999999Z",
        "--window-secs=1",
        "--count",
        path,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "10");
}

#[test]
fn parallel_count_seekztd() {
    let path = big_seekztd_fixture().to_str().unwrap();
    let out = run(&["-j=4", "--count", "--if=level=critical", path]);
    let n: usize = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    assert_eq!(n, 100_000);
}

#[test]
fn seekztd_rejects_follow() {
    let path = seekztd_fixture();
    let out = Command::new(riplog_bin())
        .arg("-F")
        .arg(path)
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("seekable-zstd") || err.contains("sealed"),
        "expected seekable-zstd follow rejection, got: {err}"
    );
}

#[test]
fn streaming_zst_warns_and_counts() {
    let path = streaming_zst_fixture();
    let out = Command::new(riplog_bin())
        .env("RUST_LOG", "warn")
        .args(["--count", path.to_str().unwrap()])
        .output()
        .expect("spawn riplog");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "200");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no seek table") || err.contains("falling back"),
        "expected streaming fallback warning, got: {err}"
    );
}

#[test]
fn streaming_zst_rejects_time_range() {
    let path = streaming_zst_fixture();
    let out = Command::new(riplog_bin())
        .args(["--time-range", path.to_str().unwrap()])
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("streaming zstd") || err.contains("--time-range"),
        "expected streaming time-range rejection, got: {err}"
    );
}

#[test]
fn streaming_zst_rejects_from_to() {
    let path = streaming_zst_fixture();
    let out = Command::new(riplog_bin())
        .args([
            "--from=2026-04-24T18:00:00Z",
            "--count",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("streaming zstd"),
        "expected streaming reject for --from, got: {err}"
    );
}
