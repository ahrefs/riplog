//! End-to-end CLI tests. Fixtures are generated on the fly by
//! `tests/generate_logs.py` with a fixed seed and start time, so output is
//! deterministic across runs.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

const SEED: &str = "42";
const COUNT: usize = 200;
const START_TIME: &str = "2026-04-24T18:00:00Z";

// Distribution of `level=` in the seed=42, count=200 fixture (precomputed).
const N_DEBUG: usize = 57;
const N_ERROR: usize = 53;
const N_INFO: usize = 49;
const N_WARN: usize = 41;

fn riplog_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_riplog"))
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Generate the standard fixture once per test process.
fn fixture_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::env::temp_dir().join("riplog-it");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("fixture-seed{SEED}-n{COUNT}.log"));
        let script = project_root().join("tests/generate_logs.py");
        // Rate=10 → 200 lines spans 20 seconds, leaves room for time-window
        // tests that pick sub-second slices.
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

fn run(args: &[&str]) -> std::process::Output {
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

fn lines(s: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(s)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn count_no_filter() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count", path]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "200");
}

#[test]
fn count_filtered_by_level() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count", "--if=level=error", path]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        N_ERROR.to_string()
    );
}

#[test]
fn count_filtered_by_regex() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count", "--if=msg=~connection", path]);
    let n: usize = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    assert!(n > 0, "expected some msg matches for `connection`, got 0");
    assert!(n < COUNT);
}

#[test]
fn list_keys_returns_expected_set() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--list-keys", path]);
    let mut keys = lines(&out.stdout);
    keys.sort();
    assert_eq!(keys, vec!["level", "msg", "time"]);
}

#[test]
fn list_values_for_level() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--list-values-for=level", path]);
    let mut levels = lines(&out.stdout);
    levels.sort();
    assert_eq!(levels, vec!["debug", "error", "info", "warn"]);
}

#[test]
fn count_by_level_matches_distribution() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count-by=level", path]);
    // Output mixes streamed log lines and the trailing count-by table. Pick
    // out only the table rows (`<count> level=<value>` after stripping
    // leading whitespace).
    let mut counts = std::collections::HashMap::new();
    for line in lines(&out.stdout) {
        let trimmed = line.trim_start();
        let mut it = trimmed.split_whitespace();
        let Some(n_token) = it.next() else { continue };
        let Ok(n) = n_token.parse::<usize>() else {
            continue;
        };
        let Some(kv) = it.next() else { continue };
        let Some(value) = kv.strip_prefix("level=") else {
            continue;
        };
        if it.next().is_some() {
            continue; // a real log line has more tokens after `level=…`
        }
        counts.insert(value.to_string(), n);
    }
    assert_eq!(counts.get("debug"), Some(&N_DEBUG));
    assert_eq!(counts.get("error"), Some(&N_ERROR));
    assert_eq!(counts.get("info"), Some(&N_INFO));
    assert_eq!(counts.get("warn"), Some(&N_WARN));
}

#[test]
fn time_range_brackets_fixture() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--time-range", path]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("2026-04-24T18:00:00"), "first ts in {s:?}");
    assert!(s.contains(" .. "), "expected range separator in {s:?}");
}

#[test]
fn time_range_with_tz_offset() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--time-range", "--tz=+02:00", path]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("2026-04-24T20:00:00"), "tz-shifted in {s:?}");
    assert!(s.contains("+02:00"), "tz suffix in {s:?}");
}

#[test]
fn from_to_filters_strictly() {
    let path = fixture_path().to_str().unwrap();
    // Fixture spans 2026-04-24T18:00:00.000000 .. 18:00:19.900000 (200 lines @ 0.1s).
    // Pick a 1-second window that contains 10 lines.
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
fn output_file_flag_writes_to_file() {
    let path = fixture_path().to_str().unwrap();
    let out_file = std::env::temp_dir().join("riplog-it-out.log");
    let _ = fs::remove_file(&out_file);
    let _ = run(&["--if=level=warn", "-o", out_file.to_str().unwrap(), path]);
    let written = fs::read_to_string(&out_file).unwrap();
    let lines_n = written.lines().count();
    assert_eq!(lines_n, N_WARN);
}

#[cfg(unix)]
fn spawn_follow(flag: &str, path: &Path) -> std::process::Child {
    Command::new(riplog_bin())
        .arg(flag)
        .arg("--count")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[cfg(unix)]
fn append_lines(path: &Path, lines: &[&str]) {
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    for line in lines {
        writeln!(f, "{line}").unwrap();
    }
}

#[cfg(unix)]
fn finish_with_sigint(child: std::process::Child) -> usize {
    let pid = child.id() as libc::pid_t;
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

const FOLLOW_TICK: Duration = Duration::from_millis(400);

#[cfg(unix)]
#[test]
fn follow_reopen_handles_rotation() {
    let path = std::env::temp_dir().join("riplog-it-follow-F.log");
    fs::write(&path, b"").unwrap();
    let child = spawn_follow("-F", &path);
    thread::sleep(FOLLOW_TICK);

    append_lines(
        &path,
        &[
            "level=info time=2026-04-24T18:00:00Z msg=appended1",
            "level=info time=2026-04-24T18:00:01Z msg=appended2",
        ],
    );
    thread::sleep(FOLLOW_TICK);

    fs::write(
        &path,
        "level=info time=2026-04-24T18:00:10Z msg=after_rotate\n",
    )
    .unwrap();
    thread::sleep(FOLLOW_TICK);

    assert_eq!(
        finish_with_sigint(child),
        3,
        "expected 2 appended + 1 post-rotation"
    );
}

#[cfg(unix)]
#[test]
fn follow_no_reopen_misses_rotation() {
    let path = std::env::temp_dir().join("riplog-it-follow-f.log");
    fs::write(&path, b"").unwrap();
    let child = spawn_follow("-f", &path);
    thread::sleep(FOLLOW_TICK);

    append_lines(
        &path,
        &[
            "level=info time=2026-04-24T18:00:00Z msg=line1",
            "level=info time=2026-04-24T18:00:01Z msg=line2",
        ],
    );
    thread::sleep(FOLLOW_TICK);

    fs::write(&path, "level=info time=2026-04-24T18:00:10Z msg=missed\n").unwrap();
    thread::sleep(FOLLOW_TICK);

    assert_eq!(
        finish_with_sigint(child),
        2,
        "-f should miss the post-rotation line"
    );
}
