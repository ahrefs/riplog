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
const N_DEBUG: usize = 54;
const N_ERROR: usize = 49;
const N_INFO: usize = 51;
const N_WARN: usize = 43;
const N_CRITICAL: usize = 3;

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
    assert_eq!(levels, vec!["critical", "debug", "error", "info", "warn"]);
}

#[test]
fn sample_rate_zero_drops_everything() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count", "--sample-rate=0", path]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");
}

#[test]
fn sample_rate_one_keeps_everything() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count", "--sample-rate=1", path]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "200");
}

#[test]
fn sample_if_scopes_the_dice_roll() {
    // Drop all `info` lines (sample-rate 0 within the sample-if subset);
    // every other level passes through untouched.
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count", "--sample-rate=0", "--sample-if=level=info", path]);
    let n: usize = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    assert_eq!(n, COUNT - N_INFO);
}

#[test]
fn sample_rate_out_of_range_errors() {
    let out = Command::new(riplog_bin())
        .args(["--sample-rate=1.5", fixture_path().to_str().unwrap()])
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success());
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
    assert_eq!(counts.get("critical"), Some(&N_CRITICAL));
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
fn limit_caps_matched_lines() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count", "--limit=5", path]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "5");
}

#[test]
fn limit_caps_streamed_output() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--limit=3", path]);
    assert_eq!(out.stdout.iter().filter(|&&b| b == b'\n').count(), 3);
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

/// Run `riplog` with `args` followed by each path stringified. Saves the
/// `&[args, &[a.to_str().unwrap(), b.to_str().unwrap()]].concat()` dance
/// across the multi-file tests below.
fn run_with_files(args: &[&str], files: &[&Path]) -> std::process::Output {
    let mut all: Vec<&str> = args.to_vec();
    all.extend(files.iter().map(|p| p.to_str().unwrap()));
    run(&all)
}

/// Split the standard fixture in half line-wise. Lines are 0.1s apart
/// starting at 18:00:00; the split boundary is at line 100 ≈ 18:00:10.
/// Cached so multiple multi-file tests share the same on-disk pair.
fn split_fixture() -> (PathBuf, PathBuf) {
    static PATHS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    PATHS
        .get_or_init(|| {
            let src = fs::read_to_string(fixture_path()).unwrap();
            let lines: Vec<&str> = src.lines().collect();
            let mid = lines.len() / 2;
            let dir = std::env::temp_dir().join("riplog-it");
            let a = dir.join("split-aa.log");
            let b = dir.join("split-ab.log");
            fs::write(&a, lines[..mid].join("\n") + "\n").unwrap();
            fs::write(&b, lines[mid..].join("\n") + "\n").unwrap();
            (a, b)
        })
        .clone()
}

#[test]
fn multi_file_count_matches_concatenation() {
    let (a, b) = split_fixture();
    let out = run_with_files(&["--count"], &[&a, &b]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        COUNT.to_string()
    );
}

#[test]
fn multi_file_count_by_aggregates_across_files() {
    let (a, b) = split_fixture();
    let multi = run_with_files(&["--count-by=level"], &[&a, &b]);
    let single = run_with_files(&["--count-by=level"], &[fixture_path()]);
    assert_eq!(
        multi.stdout, single.stdout,
        "multi and single --count-by output should be byte-identical"
    );
}

#[test]
fn multi_file_list_keys_unions() {
    let (a, b) = split_fixture();
    let out = run_with_files(&["--list-keys"], &[&a, &b]);
    let mut keys = lines(&out.stdout);
    keys.sort();
    assert_eq!(keys, vec!["level", "msg", "time"]);
}

#[test]
fn multi_file_list_values_for_unions() {
    let (a, b) = split_fixture();
    let multi = run_with_files(&["--list-values-for=level"], &[&a, &b]);
    let single = run_with_files(&["--list-values-for=level"], &[fixture_path()]);
    assert_eq!(multi.stdout, single.stdout);
}

#[test]
fn multi_file_limit_caps_globally() {
    let (a, b) = split_fixture();
    // -n 5: stops within the first file.
    let out = run_with_files(&["--count", "--limit=5"], &[&a, &b]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "5");
    // -n 150: spans both files.
    let out = run_with_files(&["--count", "--limit=150"], &[&a, &b]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "150");
    // Streamed line output also caps globally.
    let out = run_with_files(&["--limit=120"], &[&a, &b]);
    let n_lines = out.stdout.iter().filter(|&&c| c == b'\n').count();
    assert_eq!(n_lines, 120);
}

#[test]
fn multi_file_from_to_straddles_rotation() {
    let (a, b) = split_fixture();
    // Window 18:00:08..18:00:12 spans both halves (split is at 18:00:10).
    let args = [
        "--from=2026-04-24T18:00:08Z",
        "--to=2026-04-24T18:00:12Z",
        "--window-secs=1",
        "--count",
    ];
    let multi = run_with_files(&args, &[&a, &b]);
    let single = run_with_files(&args, &[fixture_path()]);
    assert_eq!(multi.stdout, single.stdout);
}

#[test]
fn multi_file_symbolic_anchors_resolve_globally() {
    // `start+5s end-5s` resolves once against the global span and applies
    // the same absolute window to both files. Result must match concatenation.
    let (a, b) = split_fixture();
    let args = ["--from=start+5s", "--to=end-5s", "--count"];
    let multi = run_with_files(&args, &[&a, &b]);
    let single = run_with_files(&args, &[fixture_path()]);
    assert_eq!(multi.stdout, single.stdout);
}

#[test]
fn multi_file_time_range_unions() {
    let (a, b) = split_fixture();
    let multi = run_with_files(&["--time-range"], &[&a, &b]);
    let single = run_with_files(&["--time-range"], &[fixture_path()]);
    assert_eq!(multi.stdout, single.stdout);
}

#[test]
fn raw_key_emits_unquoted_values_per_line() {
    let path = fixture_path().to_str().unwrap();
    // critical lines only — small, deterministic set.
    let out = run(&["--if=level=critical", "--raw-key=msg", path]);
    assert_eq!(
        out.stdout.iter().filter(|&&b| b == b'\n').count(),
        N_CRITICAL
    );
    // Every emitted line should be plain text (no `key=` prefix, no quotes).
    for line in lines(&out.stdout) {
        assert!(!line.contains('='), "raw-key line still has `=`: {line:?}");
        assert!(
            !line.starts_with('"'),
            "raw-key line still quoted: {line:?}"
        );
    }
}

#[test]
fn raw_key_unescapes_quoted_values() {
    let mut child = Command::new(riplog_bin())
        .args(["--raw-key=msg"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"time=2026-04-24T18:00:00Z msg=\"hello \\\"world\\\"\" level=info\n")
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "hello \"world\""
    );
}

#[test]
fn raw_key_skips_lines_missing_key() {
    let mut child = Command::new(riplog_bin())
        .args(["--raw-key=msg"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"msg=first\nlevel=info\nmsg=second\n")
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert_eq!(lines(&out.stdout), vec!["first", "second"]);
}

#[test]
fn stdin_rejects_seek_flags() {
    let mut child = Command::new(riplog_bin())
        .args(["--from=18:00", "--count"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("require a file argument"),
        "stderr was: {stderr}"
    );
}

#[test]
fn sort_by_orders_stdin_lines_lex() {
    let mut child = Command::new(riplog_bin())
        .args(["--sort-by=time"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            b"time=2026-04-24T18:00:02Z msg=second\n\
              time=2026-04-24T18:00:01Z msg=first\n\
              time=2026-04-24T18:00:03Z msg=third\n",
        )
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let want = "time=2026-04-24T18:00:01Z msg=first\n\
                time=2026-04-24T18:00:02Z msg=second\n\
                time=2026-04-24T18:00:03Z msg=third\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
}

#[test]
fn sort_by_with_raw_key_sorts_extracted_values() {
    let mut child = Command::new(riplog_bin())
        .args(["--sort-by=time", "--raw-key=msg"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            b"time=2026-04-24T18:00:02Z msg=B\n\
              time=2026-04-24T18:00:01Z msg=A\n\
              time=2026-04-24T18:00:03Z msg=C\n",
        )
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "A\nB\nC\n");
}

#[test]
fn sort_by_missing_key_sorts_first() {
    let mut child = Command::new(riplog_bin())
        .args(["--sort-by=tag"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            b"msg=middle tag=m\n\
              msg=last tag=z\n\
              msg=top\n",
        )
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let want = "msg=top\n\
                msg=middle tag=m\n\
                msg=last tag=z\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
}

#[test]
fn sort_by_on_file_orders_shuffled_input() {
    let dir = std::env::temp_dir().join("riplog-it");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sort-by-file.log");
    fs::write(
        &path,
        "time=2026-04-24T18:00:03Z msg=c\n\
         time=2026-04-24T18:00:01Z msg=a\n\
         time=2026-04-24T18:00:02Z msg=b\n",
    )
    .unwrap();
    let out = run(&["--sort-by=time", path.to_str().unwrap()]);
    let want = "time=2026-04-24T18:00:01Z msg=a\n\
                time=2026-04-24T18:00:02Z msg=b\n\
                time=2026-04-24T18:00:03Z msg=c\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
}

#[test]
fn sort_by_composes_with_if_filter() {
    let path = fixture_path().to_str().unwrap();
    // critical: deterministic small set. All emitted lines should still be
    // present, and msg values should be lex-sorted.
    let out = run(&["--if=level=critical", "--sort-by=msg", path]);
    let line_count = out.stdout.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(line_count, N_CRITICAL);
    // Extract the msg= token from each line and verify ascending order.
    let msgs: Vec<String> = lines(&out.stdout)
        .iter()
        .filter_map(|l| {
            l.split_whitespace()
                .find_map(|tok| tok.strip_prefix("msg=").map(str::to_string))
        })
        .collect();
    assert_eq!(msgs.len(), N_CRITICAL);
    let mut sorted = msgs.clone();
    sorted.sort();
    assert_eq!(msgs, sorted);
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
