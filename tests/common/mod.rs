//! Shared helpers for the integration test binaries. Lives at
//! `tests/common/mod.rs` so cargo treats it as a module included by each
//! test file (via `mod common;`) rather than as its own test binary. Each
//! consuming test binary uses a different subset, so silence dead-code
//! warnings module-wide rather than per-item.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub const SEED: &str = "42";
pub const COUNT: usize = 200;
pub const START_TIME: &str = "2026-04-24T18:00:00Z";

pub fn riplog_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_riplog"))
}

pub fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn run(args: &[&str]) -> std::process::Output {
    let out = Command::new(riplog_bin())
        .args(args)
        .output()
        .expect("spawn riplog");
    assert!(
        out.status.success(),
        "riplog {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

pub fn lines(s: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(s)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Standard 200-line logfmt fixture, generated once per test process via
/// `tests/generate_logs.py`. Rate=10 → 20-second span, leaves room for
/// sub-second time-window tests.
pub fn fixture_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::env::temp_dir().join("riplog-it");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("fixture-seed{SEED}-n{COUNT}.log"));
        let script = project_root().join("tests/generate_logs.py");
        let out = Command::new("python3")
            .arg(&script)
            .arg("--rate=10")
            .arg(format!("--count={COUNT}"))
            .arg(format!("--seed={SEED}"))
            .arg(format!("--start-time={START_TIME}"))
            .output()
            .expect("python3 not available, or generate_logs.py missing");
        assert!(
            out.status.success(),
            "fixture generation failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        fs::write(&path, &out.stdout).unwrap();
        path
    })
    .as_path()
}

/// ~24 MiB synthetic logfmt fixture, big enough to actually split across 4
/// workers (`MIN_BYTES_PER_WORKER * 4 = 16 MiB`). Cached for the test process.
pub fn big_fixture() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::env::temp_dir().join("riplog-it");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big-fixture.log");
        let mut buf: Vec<u8> = Vec::with_capacity(24 * 1024 * 1024);
        for i in 0..500_000u64 {
            let level = match i % 5 {
                0 => "info",
                1 => "warn",
                2 => "error",
                3 => "debug",
                _ => "critical",
            };
            let secs = i / 10;
            let frac = (i % 10) * 100_000;
            let line = format!(
                "time=2026-04-24T18:{:02}:{:02}.{:06}Z level={} msg=\"line {}\"\n",
                secs / 60 % 60,
                secs % 60,
                frac,
                level,
                i,
            );
            buf.extend_from_slice(line.as_bytes());
        }
        fs::write(&path, &buf).unwrap();
        path
    })
    .as_path()
}
