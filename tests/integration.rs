//! End-to-end CLI tests. Fixtures are generated on the fly by
//! `tests/generate_logs.py` with a fixed seed and start time, so output is
//! deterministic across runs.

mod common;

use common::{big_fixture, fixture_path, lines, riplog_bin, run, COUNT};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

// Distribution of `level=` in the seed=42, count=200 fixture (precomputed).
const N_DEBUG: usize = 54;
const N_ERROR: usize = 49;
const N_INFO: usize = 51;
const N_WARN: usize = 43;
const N_CRITICAL: usize = 3;

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

/// Parse logfmt rows of the form `count=N key.<k>=<value> ...` from `output`,
/// pulling out the `count=` field and the value of `key.<k>=`. Skip lines
/// that don't start with `count=` (full log lines that aren't part of the
/// report).
fn parse_count_rows(output: &[u8], key: &str) -> std::collections::HashMap<String, usize> {
    let mut out = std::collections::HashMap::new();
    for line in lines(output) {
        let mut it = line.split_whitespace();
        let Some(c_tok) = it.next() else { continue };
        let Some(c_str) = c_tok.strip_prefix("count=") else {
            continue;
        };
        let Ok(n) = c_str.parse::<usize>() else {
            continue;
        };
        let prefix = format!("key.{key}=");
        let mut value: Option<String> = None;
        for tok in it {
            if let Some(v) = tok.strip_prefix(&prefix) {
                value = Some(v.to_string());
                break;
            }
        }
        if let Some(v) = value {
            out.insert(v, n);
        }
    }
    out
}

#[test]
fn group_by_level_matches_distribution() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count", "--group-by=level", path]);
    let counts = parse_count_rows(&out.stdout, "level");
    assert_eq!(counts.get("debug"), Some(&N_DEBUG));
    assert_eq!(counts.get("error"), Some(&N_ERROR));
    assert_eq!(counts.get("info"), Some(&N_INFO));
    assert_eq!(counts.get("warn"), Some(&N_WARN));
    assert_eq!(counts.get("critical"), Some(&N_CRITICAL));
}

#[test]
fn group_by_without_count_errors() {
    let out = Command::new(riplog_bin())
        .args(["--group-by=level", fixture_path().to_str().unwrap()])
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success(), "expected non-zero exit");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--count"),
        "stderr should mention --count: {err}"
    );
}

#[test]
fn bucket_alone_partitions_time_range() {
    // Fixture spans 20s starting at 18:00:00 (200 lines @ 0.1s apart). A
    // 5-second bucket yields exactly 4 groups of 50 lines each, epoch-aligned.
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count", "--bucket=5s", path]);
    let mut buckets: Vec<(String, String, usize)> = Vec::new();
    for line in lines(&out.stdout) {
        let mut count: Option<usize> = None;
        let mut b_start: Option<String> = None;
        let mut b_end: Option<String> = None;
        let mut has_t_start = false;
        let mut has_t_end = false;
        for tok in line.split_whitespace() {
            if let Some(v) = tok.strip_prefix("count=") {
                count = v.parse().ok();
            } else if let Some(v) = tok.strip_prefix("bucket.start=") {
                b_start = Some(v.to_string());
            } else if let Some(v) = tok.strip_prefix("bucket.end=") {
                b_end = Some(v.to_string());
            } else if tok.starts_with("time.start=") {
                has_t_start = true;
            } else if tok.starts_with("time.end=") {
                has_t_end = true;
            }
        }
        if let (Some(c), Some(s), Some(e)) = (count, b_start, b_end) {
            assert!(has_t_start && has_t_end, "missing time range in {line:?}");
            buckets.push((s, e, c));
        }
    }
    buckets.sort();
    let want: Vec<(&str, &str)> = vec![
        ("2026-04-24T18:00:00Z", "2026-04-24T18:00:05Z"),
        ("2026-04-24T18:00:05Z", "2026-04-24T18:00:10Z"),
        ("2026-04-24T18:00:10Z", "2026-04-24T18:00:15Z"),
        ("2026-04-24T18:00:15Z", "2026-04-24T18:00:20Z"),
    ];
    let got: Vec<(&str, &str)> = buckets
        .iter()
        .map(|(s, e, _)| (s.as_str(), e.as_str()))
        .collect();
    assert_eq!(got, want);
    for (_, _, n) in &buckets {
        assert_eq!(*n, 50, "expected 50 lines per 5s bucket, got {buckets:?}");
    }
}

#[test]
fn n_buckets_partitions_window_evenly() {
    // Fixture spans [18:00:00, 18:00:19.9] (200 lines @ 0.1s apart). With
    // --from start --to end and --n-buckets=4, we expect 4 buckets aligned
    // to the window start (18:00:00). Span = 19.9s, so each bucket is 4.975s
    // wide and the start boundaries are 0, 4.975, 9.95, 14.925 seconds.
    // Verify there are exactly 4 buckets, total count = 200, and each bucket
    // is non-empty. Bucket counts sum to 200 (no line dropped).
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count", "--n-buckets=4", "--from=start", "--to=end", path]);
    let mut total = 0usize;
    let mut n_rows = 0usize;
    for line in lines(&out.stdout) {
        let mut count: Option<usize> = None;
        let mut has_b_start = false;
        let mut has_b_end = false;
        for tok in line.split_whitespace() {
            if let Some(v) = tok.strip_prefix("count=") {
                count = v.parse().ok();
            } else if tok.starts_with("bucket.start=") {
                has_b_start = true;
            } else if tok.starts_with("bucket.end=") {
                has_b_end = true;
            }
        }
        if let Some(c) = count {
            assert!(
                has_b_start && has_b_end,
                "row missing bucket boundaries: {line:?}"
            );
            total += c;
            n_rows += 1;
        }
    }
    assert_eq!(n_rows, 4, "expected 4 buckets");
    // Last fixture line (18:00:19.9) sits past 4 * (19.9/4) = 19.9 — so 200
    // matched lines fall into the 4 buckets (the last line equals the upper
    // bound, which is included).
    assert_eq!(total, 200);
}

#[test]
fn n_buckets_without_window_uses_file_range() {
    // No --from/--to: the window comes from the file's first/last
    // timestamps. 200 lines / 4 buckets = 50 lines per bucket exactly when
    // the divide lines up.
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--count", "--n-buckets=4", path]);
    let mut total = 0usize;
    let mut n_rows = 0usize;
    for line in lines(&out.stdout) {
        for tok in line.split_whitespace() {
            if let Some(v) = tok.strip_prefix("count=") {
                let c: usize = v.parse().unwrap();
                total += c;
                n_rows += 1;
            }
        }
    }
    assert_eq!(n_rows, 4);
    assert_eq!(total, 200);
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

/// Big fixture with strictly monotone timestamps spanning 1+ hour, just
/// large enough to trigger parallel chunk splitting (≥ `MIN_BYTES_PER_WORKER`
/// * 4 = 16 MiB). Used for streaming tests where the close-on-`max_ts >
/// end+grace` invariant requires that timestamps don't wrap.
fn big_monotone_fixture() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::env::temp_dir().join("riplog-it");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big-monotone-fixture.log");
        // 300_000 lines @ 100/sec → 3000s = 50 minutes. ~18 MiB on disk.
        let mut buf: Vec<u8> = Vec::with_capacity(20 * 1024 * 1024);
        for i in 0..300_000u64 {
            let level = match i % 5 {
                0 => "info",
                1 => "warn",
                2 => "error",
                3 => "debug",
                _ => "critical",
            };
            let total_ms = i * 10;
            let secs = total_ms / 1000;
            let ms = total_ms % 1000;
            let h = 18 + secs / 3600;
            let m = (secs / 60) % 60;
            let s = secs % 60;
            let line = format!(
                "time=2026-04-24T{h:02}:{m:02}:{s:02}.{ms:03}Z level={level} msg=\"line {i}\"\n"
            );
            buf.extend_from_slice(line.as_bytes());
        }
        fs::write(&path, &buf).unwrap();
        path
    })
    .as_path()
}

#[cfg(unix)]
#[test]
fn follow_with_parallel_does_not_interleave() {
    // Multi-file follow with -j: pre-rotation file is parallel-scanned,
    // last file is followed. Workers must batch (not stream) — otherwise
    // their bucket rows would interleave on the shared writer. Compare
    // parallel vs sequential output: must be byte-identical at SIGINT.
    let prerot = big_monotone_fixture();
    let last_path = std::env::temp_dir().join("riplog-it-follow-par-last.log");
    fs::write(&last_path, b"").unwrap();

    fn run_once(j: Option<&str>, prerot: &Path, last: &Path) -> Vec<u8> {
        let mut cmd = Command::new(riplog_bin());
        if let Some(j) = j {
            cmd.arg(format!("-j={j}"));
        }
        cmd.args([
            "-F",
            "--count",
            "--group-by=level",
            "--bucket=10m",
            prerot.to_str().unwrap(),
            last.to_str().unwrap(),
        ]);
        let child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Sequential scan of an 18-MiB fixture in debug builds takes
        // ~1.5–2s. Wait long enough for both runs to finish the initial
        // scan and reach the follow-poll loop before SIGINT'ing — otherwise
        // sequential's mid-scan stream buffer differs from parallel's
        // post-merge flush_remaining.
        thread::sleep(Duration::from_secs(3));
        let pid = child.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }
        child.wait_with_output().unwrap().stdout
    }

    let seq = run_once(None, prerot, &last_path);
    let par = run_once(Some("4"), prerot, &last_path);
    assert_eq!(
        seq, par,
        "parallel follow output must match sequential — workers should not stream"
    );
}

#[cfg(unix)]
#[test]
fn follow_bucket_emits_on_close() {
    // Stream bucketed counts as buckets close. With --bucket=2s
    // --window-secs=1, bucket A=[18:00:00, 18:00:02) closes once a line
    // arrives whose ts > 18:00:03 (A.end + window). Append 3 lines in A,
    // then 2 lines in B=[18:00:04, 18:00:06) — that triggers A's flush.
    // SIGINT then flushes B in flush_remaining.
    let path = std::env::temp_dir().join("riplog-it-follow-bucket.log");
    fs::write(&path, b"").unwrap();
    let child = Command::new(riplog_bin())
        .args([
            "-F",
            "--count",
            "--bucket=2s",
            "--window-secs=1",
            path.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(FOLLOW_TICK);

    append_lines(
        &path,
        &[
            "time=2026-04-24T18:00:00.1Z level=info msg=a1",
            "time=2026-04-24T18:00:00.5Z level=info msg=a2",
            "time=2026-04-24T18:00:01.5Z level=info msg=a3",
        ],
    );
    thread::sleep(FOLLOW_TICK);

    append_lines(
        &path,
        &[
            "time=2026-04-24T18:00:04.1Z level=info msg=b1",
            "time=2026-04-24T18:00:04.5Z level=info msg=b2",
        ],
    );
    thread::sleep(FOLLOW_TICK);

    let pid = child.id() as libc::pid_t;
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<&str> = stdout.lines().filter(|l| l.starts_with("count=")).collect();
    assert_eq!(rows.len(), 2, "expected 2 bucket rows, got: {stdout:?}");
    assert!(
        rows[0].contains("count=3") && rows[0].contains("bucket.start=2026-04-24T18:00:00Z"),
        "first row should be bucket A with count 3: {:?}",
        rows[0]
    );
    assert!(
        rows[1].contains("count=2") && rows[1].contains("bucket.start=2026-04-24T18:00:04Z"),
        "second row should be bucket B with count 2: {:?}",
        rows[1]
    );
}

#[test]
fn follow_rejects_group_by_without_bucket() {
    let path = std::env::temp_dir().join("riplog-it-follow-rej-gb.log");
    fs::write(&path, b"").unwrap();
    let out = Command::new(riplog_bin())
        .args(["-f", "--count", "--group-by=level", path.to_str().unwrap()])
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success(), "expected non-zero exit");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--bucket"),
        "stderr should mention --bucket: {err}"
    );
}

#[test]
fn follow_rejects_n_buckets() {
    let path = std::env::temp_dir().join("riplog-it-follow-rej-nb.log");
    fs::write(&path, b"").unwrap();
    let out = Command::new(riplog_bin())
        .args(["-f", "--count", "--n-buckets=10", path.to_str().unwrap()])
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success(), "expected non-zero exit");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("-f") || err.contains("-F"),
        "stderr should mention -f/-F: {err}"
    );
}

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

/// `riplog rotated.log current.log -F`: both files must be read in full
/// before the follow loop attaches to `current.log` (the rotation use case).
/// Regression for a bug where the last file's `start_byte` was set to EOF,
/// silently skipping its pre-existing content.
#[cfg(unix)]
#[test]
fn follow_multi_file_reads_last_fully_then_tails() {
    let dir = std::env::temp_dir().join("riplog-it-follow-multi");
    fs::create_dir_all(&dir).unwrap();
    let rotated = dir.join("rotated.log");
    let current = dir.join("current.log");
    fs::write(
        &rotated,
        "level=info time=2026-04-24T18:00:00Z msg=r1\n\
         level=info time=2026-04-24T18:00:01Z msg=r2\n",
    )
    .unwrap();
    fs::write(
        &current,
        "level=info time=2026-04-24T18:00:02Z msg=c1\n\
         level=info time=2026-04-24T18:00:03Z msg=c2\n",
    )
    .unwrap();

    let child = Command::new(riplog_bin())
        .args(["-F", "--count"])
        .arg(&rotated)
        .arg(&current)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(FOLLOW_TICK);

    append_lines(
        &current,
        &["level=info time=2026-04-24T18:00:10Z msg=after1"],
    );
    thread::sleep(FOLLOW_TICK);

    assert_eq!(
        finish_with_sigint(child),
        5,
        "expected 2 rotated + 2 current + 1 appended"
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
fn multi_file_group_by_aggregates_across_files() {
    let (a, b) = split_fixture();
    let multi = run_with_files(&["--count", "--group-by=level"], &[&a, &b]);
    let single = run_with_files(&["--count", "--group-by=level"], &[fixture_path()]);
    assert_eq!(
        multi.stdout, single.stdout,
        "multi and single --group-by output should be byte-identical"
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
fn parallel_count_matches_sequential() {
    let path = big_fixture().to_str().unwrap();
    let seq = run(&["--count", "--if=level=critical", path]);
    let par = run(&["-j=4", "--count", "--if=level=critical", path]);
    assert_eq!(seq.stdout, par.stdout);
    let n: usize = String::from_utf8_lossy(&par.stdout).trim().parse().unwrap();
    assert_eq!(n, 100_000); // 500k / 5 levels
}

#[test]
fn parallel_group_by_matches_sequential() {
    let path = big_fixture().to_str().unwrap();
    let seq = run(&["--count", "--group-by=level", path]);
    let par = run(&["-j=4", "--count", "--group-by=level", path]);
    assert_eq!(seq.stdout, par.stdout);
}

#[test]
fn parallel_bucket_matches_sequential() {
    // big_fixture spans ~50_000 seconds; 10-minute buckets give ~83 buckets,
    // comfortably more than -j=4 workers, so the bucket-aligned parallel path
    // fires (rather than falling back to byte-midpoint chunking). Output must
    // be byte-identical to the sequential run.
    let path = big_fixture().to_str().unwrap();
    let seq = run(&["--count", "--group-by=level", "--bucket=10m", path]);
    let par = run(&["-j=4", "--count", "--group-by=level", "--bucket=10m", path]);
    assert_eq!(seq.stdout, par.stdout);
}

#[test]
fn parallel_n_buckets_matches_sequential() {
    // --n-buckets path: window comes from the file's first/last timestamps,
    // 60 buckets / 4 workers = 15 buckets per worker. Sequential and parallel
    // outputs must match.
    let path = big_fixture().to_str().unwrap();
    let seq = run(&["--count", "--group-by=level", "--n-buckets=60", path]);
    let par = run(&[
        "-j=4",
        "--count",
        "--group-by=level",
        "--n-buckets=60",
        path,
    ]);
    assert_eq!(seq.stdout, par.stdout);
}

#[test]
fn parallel_lines_match_sorted_sequential() {
    let path = big_fixture().to_str().unwrap();
    let seq = run(&["--if=level=critical", path]);
    let par = run(&["-j=4", "--if=level=critical", path]);
    let mut seq_lines: Vec<&[u8]> = seq.stdout.split(|&b| b == b'\n').collect();
    let mut par_lines: Vec<&[u8]> = par.stdout.split(|&b| b == b'\n').collect();
    seq_lines.sort_unstable();
    par_lines.sort_unstable();
    assert_eq!(seq_lines, par_lines);
}

#[test]
fn parallel_with_sort_by_matches_sequential() {
    let path = big_fixture().to_str().unwrap();
    let seq = run(&["--if=level=critical", "--sort-by=time", path]);
    let par = run(&["-j=4", "--if=level=critical", "--sort-by=time", path]);
    assert_eq!(seq.stdout, par.stdout);
}

#[test]
fn parallel_rejects_limit() {
    let path = big_fixture().to_str().unwrap();
    let out = Command::new(riplog_bin())
        .args(["-j=4", "-n=10", path])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot be combined"), "stderr was: {err}");
}

#[test]
fn parallel_falls_back_on_small_input() {
    // Standard fixture is < MIN_BYTES_PER_WORKER, so -j=4 falls back to a
    // single-chunk pass; output should be identical to sequential.
    let path = fixture_path().to_str().unwrap();
    let seq = run(&["--count", path]);
    let par = run(&["-j=4", "--count", path]);
    assert_eq!(seq.stdout, par.stdout);
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
fn stdin_bucket_streams_in_time_order() {
    // Stdin + --bucket activates streaming aggregation: rows come out in
    // bucket-asc, count-desc-within-bucket order. Today's batch path
    // (Counter::report) would sort globally by count desc, which would
    // produce a clearly different sequence — so this test is sensitive to
    // the streaming code path being wired up for stdin.
    //
    // Three buckets, intentionally lopsided per bucket so count-desc within
    // a bucket flips the order across buckets:
    //   A (18:00:00..01): 3×info + 2×error  -> info first
    //   B (18:00:01..02): 4×error + 1×info  -> error first
    //   C (18:00:02..03): 5×info             -> info only
    let mut child = Command::new(riplog_bin())
        .args([
            "--count",
            "--group-by=level",
            "--bucket=1s",
            "--window-secs=1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let input = b"\
time=2026-04-24T18:00:00Z level=info msg=a\n\
time=2026-04-24T18:00:00Z level=error msg=b\n\
time=2026-04-24T18:00:00Z level=info msg=a\n\
time=2026-04-24T18:00:00Z level=error msg=b\n\
time=2026-04-24T18:00:00Z level=info msg=a\n\
time=2026-04-24T18:00:01Z level=error msg=b\n\
time=2026-04-24T18:00:01Z level=info msg=a\n\
time=2026-04-24T18:00:01Z level=error msg=b\n\
time=2026-04-24T18:00:01Z level=error msg=b\n\
time=2026-04-24T18:00:01Z level=error msg=b\n\
time=2026-04-24T18:00:02Z level=info msg=a\n\
time=2026-04-24T18:00:02Z level=info msg=a\n\
time=2026-04-24T18:00:02Z level=info msg=a\n\
time=2026-04-24T18:00:02Z level=info msg=a\n\
time=2026-04-24T18:00:02Z level=info msg=a\n";
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "stderr: {:?}", out.stderr);
    let want = "\
count=3 key.level=info bucket.start=2026-04-24T18:00:00Z bucket.end=2026-04-24T18:00:01Z time.start=2026-04-24T18:00:00Z time.end=2026-04-24T18:00:00Z\n\
count=2 key.level=error bucket.start=2026-04-24T18:00:00Z bucket.end=2026-04-24T18:00:01Z time.start=2026-04-24T18:00:00Z time.end=2026-04-24T18:00:00Z\n\
count=4 key.level=error bucket.start=2026-04-24T18:00:01Z bucket.end=2026-04-24T18:00:02Z time.start=2026-04-24T18:00:01Z time.end=2026-04-24T18:00:01Z\n\
count=1 key.level=info bucket.start=2026-04-24T18:00:01Z bucket.end=2026-04-24T18:00:02Z time.start=2026-04-24T18:00:01Z time.end=2026-04-24T18:00:01Z\n\
count=5 key.level=info bucket.start=2026-04-24T18:00:02Z bucket.end=2026-04-24T18:00:03Z time.start=2026-04-24T18:00:02Z time.end=2026-04-24T18:00:02Z\n";
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

#[test]
fn add_appends_pairs_at_end() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--limit", "1", "--add", "tag=ok", path]);
    let line = lines(&out.stdout).into_iter().next().unwrap();
    assert!(
        line.contains(" tag=ok"),
        "expected appended pair, got {:?}",
        line
    );
}

#[test]
fn add_comma_separates_pairs() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--limit", "1", "--add=tag=ok,extra=2", path]);
    let line = lines(&out.stdout).into_iter().next().unwrap();
    assert!(
        line.contains(" tag=ok") && line.contains(" extra=2"),
        "got {:?}",
        line
    );
}

#[test]
fn add_conflicts_with_raw_key() {
    let path = fixture_path().to_str().unwrap();
    let out = Command::new(riplog_bin())
        .args(["--raw-key", "msg", "--add", "msg=hello", path])
        .output()
        .expect("spawn riplog");
    assert!(
        !out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("conflicts with `--raw-key`"),
        "stderr should mention conflict: {err}"
    );
}

#[test]
fn rm_drops_field() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--limit", "1", "--rm", "msg", path]);
    let line = lines(&out.stdout).into_iter().next().unwrap();
    assert!(!line.contains("msg="), "got {:?}", line);
}

#[test]
fn rm_conflicts_with_group_by() {
    let path = fixture_path().to_str().unwrap();
    let out = Command::new(riplog_bin())
        .args(["--count", "--group-by", "level", "--rm", "level", path])
        .output()
        .expect("spawn riplog");
    assert!(
        !out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
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

// ---------------- `--json` (JSONL output) ----------------

#[test]
fn json_line_output_each_line_parses() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--limit=10", "--json", path]);
    let text = String::from_utf8(out.stdout).unwrap();
    let parsed: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("each JSONL line must parse"))
        .collect();
    assert_eq!(parsed.len(), 10);
    for v in &parsed {
        let obj = v.as_object().expect("each line is a JSON object");
        // The fixture always has these fields.
        assert!(obj.contains_key("time"));
        assert!(obj.contains_key("level"));
        // Every value is a JSON string (no type coercion).
        for (_, val) in obj.iter() {
            assert!(val.is_string(), "expected string, got {val}");
        }
    }
}

#[test]
fn json_line_output_preserves_logfmt_order() {
    // First line of the fixture has well-known field order.
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--limit=1", "--json", path]);
    let line = String::from_utf8(out.stdout).unwrap();
    // Indices in the raw string: `time` must come before `level`.
    let idx_time = line.find("\"time\"").unwrap();
    let idx_level = line.find("\"level\"").unwrap();
    assert!(idx_time < idx_level, "key order not preserved: {line}");
}

#[test]
fn json_bare_count_is_object() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--json", "--count", path]);
    let line = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["count"].as_u64(), Some(COUNT as u64));
}

#[test]
fn json_aggregation_row_nests_keys() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--json", "--count", "--group-by=level", path]);
    let text = String::from_utf8(out.stdout).unwrap();
    let mut total = 0u64;
    for line in text.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let c = v["count"].as_u64().expect("count is u64");
        total += c;
        assert!(v["keys"].is_object(), "keys object missing on: {line}");
        assert!(
            v["keys"]["level"].is_string(),
            "keys.level missing on: {line}"
        );
        assert!(v["key.level"].is_null(), "stray key.level on: {line}");
    }
    assert_eq!(total, COUNT as u64);
}

#[test]
fn json_bare_count_has_no_keys_field() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--json", "--count", path]);
    let line = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert!(
        v.get("keys").is_none(),
        "keys field should be omitted when no --group-by: {line}"
    );
}

#[test]
fn json_aggregation_with_bucket_has_bucket_fields() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--json", "--count", "--group-by=level", "--bucket=5s", path]);
    let text = String::from_utf8(out.stdout).unwrap();
    let first = text.lines().next().expect("at least one row");
    let v: serde_json::Value = serde_json::from_str(first).unwrap();
    assert!(v["bucket.start"].is_string());
    assert!(v["bucket.end"].is_string());
    assert!(v["time.start"].is_string());
    assert!(v["time.end"].is_string());
}

#[test]
fn json_list_keys_is_array_of_strings() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--json", "--list-keys", path]);
    let text = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    let arr = v.as_array().expect("JSON array");
    assert!(arr.iter().all(|x| x.is_string()));
    let strs: Vec<&str> = arr.iter().map(|x| x.as_str().unwrap()).collect();
    assert!(strs.contains(&"level"));
    assert!(strs.contains(&"time"));
    // Sorted, like the logfmt variant.
    let mut sorted = strs.clone();
    sorted.sort_unstable();
    assert_eq!(strs, sorted);
}

#[test]
fn json_list_values_for_single_is_array_of_strings() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--json", "--list-values-for=level", path]);
    let text = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    let arr = v.as_array().expect("JSON array");
    assert!(arr.iter().all(|x| x.is_string()));
    let strs: Vec<&str> = arr.iter().map(|x| x.as_str().unwrap()).collect();
    for expected in &["critical", "debug", "error", "info", "warn"] {
        assert!(strs.contains(expected), "missing {expected} in {strs:?}");
    }
}

#[test]
fn json_list_values_for_multi_is_array_of_objects() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&["--json", "--list-values-for=level,msg", path]);
    let text = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    let arr = v.as_array().expect("JSON array");
    assert!(!arr.is_empty());
    // Every element: `{"key": "...", "value": "..."}`.
    for e in arr {
        let obj = e.as_object().expect("element is object");
        assert!(obj["key"].is_string());
        assert!(obj["value"].is_string());
    }
    // Both keys must appear.
    let keys: std::collections::HashSet<&str> =
        arr.iter().map(|e| e["key"].as_str().unwrap()).collect();
    assert!(keys.contains("level"));
    assert!(keys.contains("msg"));
}

#[test]
fn json_conflicts_with_raw_key() {
    let path = fixture_path().to_str().unwrap();
    let out = Command::new(riplog_bin())
        .args(["--json", "--raw-key=msg", path])
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("cannot be used with"),
        "stderr should mention conflict: {err}"
    );
}

#[test]
fn json_conflicts_with_color_always() {
    let path = fixture_path().to_str().unwrap();
    let out = Command::new(riplog_bin())
        .args(["--json", "--color=always", path])
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--color=always"),
        "stderr should mention conflict: {err}"
    );
}

#[test]
fn json_with_add_and_rm() {
    let path = fixture_path().to_str().unwrap();
    let out = run(&[
        "--limit=1",
        "--json",
        "--add=extra=hi",
        "--rm",
        "time",
        path,
    ]);
    let line = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let obj = v.as_object().unwrap();
    assert!(!obj.contains_key("time"), "time should be removed");
    assert_eq!(obj["extra"].as_str(), Some("hi"));
}

#[test]
fn json_stdin_bucket_streaming() {
    // Feed a deterministic mini-stream; assert at least one bucket row
    // emits as JSONL with the expected shape.
    let mut child = Command::new(riplog_bin())
        .args(["--json", "--count", "--group-by=level", "--bucket=1s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = String::new();
    for i in 0..5 {
        input.push_str(&format!(
            "time=2026-04-24T18:00:{:02}Z level=info msg=hello\n",
            i
        ));
    }
    // Drive a few buckets forward so the early ones close.
    for i in 30..32 {
        input.push_str(&format!(
            "time=2026-04-24T18:00:{:02}Z level=info msg=hello\n",
            i
        ));
    }
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.is_empty(), "no JSONL rows emitted");
    for line in text.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(v["count"].is_number());
        assert!(v["bucket.start"].is_string());
    }
}

// ---------------- `-` (stdin alias in file list) ----------------

/// Run riplog with `args` and feed `stdin_in` on stdin. Asserts success.
fn run_with_stdin(args: &[&str], stdin_in: &[u8]) -> std::process::Output {
    let mut child = Command::new(riplog_bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn riplog");
    child.stdin.as_mut().unwrap().write_all(stdin_in).unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "riplog {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn stdin_dash_alone_equivalent_to_no_files() {
    let input = b"time=2026-04-24T18:00:00Z level=info msg=a\n\
                  time=2026-04-24T18:00:01Z level=warn msg=b\n";
    let with_dash = run_with_stdin(&["-"], input);
    let without_dash = run_with_stdin(&[], input);
    assert_eq!(with_dash.stdout, without_dash.stdout);
}

#[test]
fn stdin_dash_after_file_streams_both() {
    let path = fixture_path().to_str().unwrap();
    let stdin_in = b"time=2026-04-24T18:00:00Z level=info msg=fromstdin\n";
    // Only one matching critical line in the fixture; stdin adds one info line.
    let out = run_with_stdin(&["--if=level=info", path, "-"], stdin_in);
    let lines = lines(&out.stdout);
    // Last line must be the stdin-injected one (file streams first, then stdin).
    assert!(
        lines.last().unwrap().contains("fromstdin"),
        "expected stdin line last, got: {lines:?}"
    );
}

#[test]
fn stdin_dash_count_aggregates_across_sources() {
    let path = fixture_path().to_str().unwrap();
    let stdin_in = b"time=2026-04-24T18:00:00Z level=info msg=a\n\
                     time=2026-04-24T18:00:01Z level=warn msg=b\n";
    let out = run_with_stdin(&["--count", path, "-"], stdin_in);
    let n: usize = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("count is an integer");
    assert_eq!(n, COUNT + 2);
}

#[test]
fn stdin_dash_group_by_aggregates_across_sources() {
    let path = fixture_path().to_str().unwrap();
    // Add a synthetic `level=info` line via stdin; the resulting info count
    // should be the fixture's info count plus 1.
    let stdin_in = b"time=2026-04-24T18:00:00Z level=info msg=extra\n";
    let out = run_with_stdin(&["--count", "--group-by=level", path, "-"], stdin_in);
    let text = String::from_utf8_lossy(&out.stdout);
    let info_line = text
        .lines()
        .find(|l| l.contains("key.level=info"))
        .expect("info row");
    // count=<N> is the first token; parse it.
    let count: usize = info_line["count=".len()..]
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(count, N_INFO + 1);
}

#[test]
fn stdin_dash_rejects_follow() {
    let path = fixture_path().to_str().unwrap();
    let out = Command::new(riplog_bin())
        .args(["-F", path, "-"])
        .stdin(Stdio::null())
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("`-` (stdin)"),
        "expected stdin-conflict message, got: {err}"
    );
}

#[test]
fn stdin_dash_rejects_time_range() {
    let path = fixture_path().to_str().unwrap();
    let out = Command::new(riplog_bin())
        .args(["--time-range", path, "-"])
        .stdin(Stdio::null())
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("`-` (stdin)"), "got: {err}");
}

#[test]
fn stdin_dash_duplicate_rejected() {
    let out = Command::new(riplog_bin())
        .args(["-", "-"])
        .stdin(Stdio::null())
        .output()
        .expect("spawn riplog");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("more than once"), "got: {err}");
}

#[test]
fn stdin_dash_from_resolves_against_real_file() {
    // With `--from start`, `start` anchors on the real file's first timestamp
    // (2026-04-24T18:00:00Z, per fixture). The stdin-injected line at
    // 18:00:00Z passes; the one before it (17:59:59Z) is filtered out.
    let path = fixture_path().to_str().unwrap();
    let stdin_in = b"time=2026-04-24T17:59:59Z level=info msg=before\n\
                     time=2026-04-24T18:00:00Z level=info msg=after\n";
    let out = run_with_stdin(&["--from=start", "--if=msg=~stdin", path, "-"], stdin_in);
    // Neither stdin line matches the regex; this just smoke-checks that the
    // run completes (i.e., --from + stdin doesn't bail).
    assert!(out.status.success());

    // Now actually look at the stdin lines: `before` should be filtered out
    // (before the resolved `start`), `after` should pass.
    let out = run_with_stdin(&["--from=start", "--if=level=info", path, "-"], stdin_in);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("msg=after"), "stdin 'after' line missing");
    assert!(
        !text.contains("msg=before"),
        "stdin 'before' line should be filtered by --from"
    );
}

#[test]
fn stdin_dash_parallel_falls_back_to_sequential() {
    // `-j` should still work when `-` is in the file list. Real file uses
    // workers; stdin runs sequentially. Just smoke-test the combination.
    let path = fixture_path().to_str().unwrap();
    let stdin_in = b"time=2026-04-24T18:00:00Z level=info msg=fromstdin\n";
    let out = run_with_stdin(&["-j", "--count", path, "-"], stdin_in);
    let n: usize = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    assert_eq!(n, COUNT + 1);
}
