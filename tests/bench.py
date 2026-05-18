#!/usr/bin/env python3
"""Performance regression harness for the `riplog` CLI.

Builds the release binary plus the `gen_logs` example, generates (or
verifies) a ~500 MB fixture, then drives `perf stat -j` over a matrix of
named CLI configurations, comparing instruction counts to a baseline.

Usage:
  python3 tests/bench.py                 # run, compare to baseline
  python3 tests/bench.py --list          # list config names
  python3 tests/bench.py --only NAME     # run one config
  python3 tests/bench.py --update-baseline
  python3 tests/bench.py --threshold-pct 3

No third-party dependencies.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Optional

REPO_ROOT = Path(__file__).resolve().parent.parent
FIXTURE_DIR = REPO_ROOT / "target" / "bench-fixtures"
FIXTURE_PATH = FIXTURE_DIR / "big.log"
BASELINE_PATH = REPO_ROOT / "tests" / "bench-baseline.json"
BIN_PATH = REPO_ROOT / "target" / "release" / "riplog"
GEN_PATH = REPO_ROOT / "target" / "release" / "examples" / "gen_logs"

FIXTURE_LINES = 8_000_000  # ~513 MB at ~64 B/line
FIXTURE_SEED = 42
FIXTURE_START = "2026-01-01T00:00:00Z"
FIXTURE_RATE = 1000.0

PERF_EVENTS = ["task-clock", "instructions", "cycles", "branch-misses", "cache-misses"]
PERF_REPEATS = 5
DEFAULT_THRESHOLD_PCT = 2.0

# Bench matrix. Each entry: (name, args list to append after the fixture path).
CONFIGS: list[tuple[str, list[str]]] = [
    ("passthrough_no_filter", []),
    ("filter_level_error", ["--if", "level=error"]),
    ("count_only", ["--count"]),
    ("group_by_level", ["--count", "--group-by", "level"]),
    ("bucket_1m", ["--count", "--bucket", "1m"]),
    ("n_buckets_10", ["--count", "--n-buckets", "10", "--from", "start", "--to", "end"]),
    ("parallel_4", ["-j=4", "--if", "level=error"]),
    ("json_emit", ["--json", "--if", "level=error"]),
    ("sort_by_time", ["--sort-by", "time", "--if", "level=error"]),
    (
        "bisect_window",
        ["--from", "2026-01-01T00:00:00Z", "--to", "2026-01-01T01:00:00Z"],
    ),
]


# ---------------------------------------------------------------------------
# Permissions / environment checks
# ---------------------------------------------------------------------------

def check_perf_paranoid() -> None:
    p = Path("/proc/sys/kernel/perf_event_paranoid")
    if not p.exists():
        print("error: /proc/sys/kernel/perf_event_paranoid not found; this script needs Linux perf.", file=sys.stderr)
        sys.exit(2)
    try:
        val = int(p.read_text().strip())
    except Exception as e:
        print(f"error: could not read {p}: {e}", file=sys.stderr)
        sys.exit(2)
    if val > 2:
        print(
            f"error: kernel.perf_event_paranoid={val} is too restrictive for `perf stat`.\n"
            "  Lower it temporarily:\n"
            "    sudo sysctl kernel.perf_event_paranoid=1\n"
            "  Or grant the binary the perfmon capability:\n"
            f"    sudo setcap cap_perfmon+ep {BIN_PATH}",
            file=sys.stderr,
        )
        sys.exit(2)


def check_tools() -> None:
    for tool in ("perf", "cargo"):
        if shutil.which(tool) is None:
            print(f"error: `{tool}` not found in PATH", file=sys.stderr)
            sys.exit(2)


# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

def cargo_build() -> None:
    print("[build] cargo build --release", flush=True)
    subprocess.run(
        ["cargo", "build", "--release"],
        cwd=REPO_ROOT,
        check=True,
    )
    print("[build] cargo build --release --example gen_logs", flush=True)
    subprocess.run(
        ["cargo", "build", "--release", "--example", "gen_logs"],
        cwd=REPO_ROOT,
        check=True,
    )


# ---------------------------------------------------------------------------
# Fixture management
# ---------------------------------------------------------------------------

def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while True:
            chunk = f.read(1 << 20)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def generate_fixture() -> None:
    FIXTURE_DIR.mkdir(parents=True, exist_ok=True)
    print(
        f"[fixture] generating {FIXTURE_LINES:,} lines -> {FIXTURE_PATH}",
        flush=True,
    )
    cmd = [
        str(GEN_PATH),
        "--count", str(FIXTURE_LINES),
        "--seed", str(FIXTURE_SEED),
        "--start-time", FIXTURE_START,
        "--rate", str(FIXTURE_RATE),
        "--out", str(FIXTURE_PATH),
    ]
    subprocess.run(cmd, check=True)
    size = FIXTURE_PATH.stat().st_size
    print(f"[fixture] wrote {size:,} bytes ({size / 1e6:.1f} MB)", flush=True)


def ensure_fixture(baseline: dict) -> str:
    """Ensure the fixture exists and its sha matches the baseline. Returns sha."""
    if not FIXTURE_PATH.exists():
        generate_fixture()
    sha = sha256_file(FIXTURE_PATH)
    expected = baseline.get("fixture_sha256")
    if expected is None:
        # First-time setup; record it.
        baseline["fixture_sha256"] = sha
        print(f"[fixture] recording sha256: {sha}", flush=True)
        return sha
    if sha != expected:
        print(
            f"[fixture] sha mismatch (have {sha}, baseline {expected}); regenerating",
            flush=True,
        )
        FIXTURE_PATH.unlink()
        generate_fixture()
        sha = sha256_file(FIXTURE_PATH)
        if sha != expected:
            print(
                f"error: regenerated fixture sha {sha} still differs from baseline "
                f"{expected}. The generator output may have changed; use "
                "--update-baseline if intentional.",
                file=sys.stderr,
            )
            sys.exit(3)
    return sha


# ---------------------------------------------------------------------------
# perf stat -j parsing
# ---------------------------------------------------------------------------

def _parse_perf_value(v: object) -> Optional[float]:
    """Coerce a perf JSON value to float. Handles strings, ints, floats, and
    the sentinel '<not counted>' / '<not supported>' values."""
    if v is None:
        return None
    if isinstance(v, (int, float)):
        return float(v)
    if isinstance(v, str):
        s = v.strip()
        if s.startswith("<") and s.endswith(">"):
            return None
        # Some perf builds emit thousand separators or units; strip non-num.
        s = s.replace(",", "").replace("_", "")
        try:
            return float(s)
        except ValueError:
            return None
    return None


def parse_perf_json_stderr(text: str) -> dict[str, dict[str, float]]:
    """Parse `perf stat -j` stderr output.

    Each non-empty line is a JSON object with keys that vary by perf version.
    We tolerate:
      - "event"/"event-name"/"event_name"  for the event label
      - "counter-value"/"value"            for the count
      - "stddev"/"std"                     for the stddev (optional)
      - "<not counted>"/"<not supported>"  surfaced as warnings

    Returns a dict event_name -> {"value": float, "stddev": float | nan}.
    Lines that fail to parse are logged to stderr and skipped.
    """
    out: dict[str, dict[str, float]] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        if not line.startswith("{"):
            # perf sometimes prints a header line; ignore.
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            print(f"[perf] skipping unparseable line: {line!r}", file=sys.stderr)
            continue
        event = (
            obj.get("event")
            or obj.get("event-name")
            or obj.get("event_name")
        )
        if not event:
            print(f"[perf] no event field in row: {line!r}", file=sys.stderr)
            continue
        # Strip the user/kernel-mode modifier perf appends when paranoid>=2
        # restricts to user-only counters (e.g. `instructions:u`). Keep the
        # event identity stable so callers can look up by the unqualified name.
        if ":" in event:
            event = event.split(":", 1)[0]
        val = _parse_perf_value(
            obj.get("counter-value")
            if "counter-value" in obj
            else obj.get("value")
        )
        # Some perf versions store the sentinel in a separate field.
        if val is None:
            sentinel = obj.get("counter-value") or obj.get("value")
            print(
                f"[perf] warning: event {event!r} not counted (raw: {sentinel!r})",
                file=sys.stderr,
            )
            continue
        stddev_raw = obj.get("stddev")
        if stddev_raw is None:
            stddev_raw = obj.get("std")
        stddev = _parse_perf_value(stddev_raw)
        out[event] = {
            "value": val,
            "stddev": stddev if stddev is not None else float("nan"),
        }
    return out


# Self-test the perf parser at module load time, so we catch regressions in
# CI even when not running the full bench. We exercise two known formats.
def _selftest_perf_parser() -> None:
    import io
    import contextlib
    sample_new = (
        '{"counter-value": "1234567890", "unit": "", "event": "instructions", '
        '"event-runtime": 1000000, "pcnt-running": 100.00, "stddev": "0.50"}\n'
        '{"counter-value": "987654321", "unit": "", "event": "cycles", '
        '"stddev": "0.10"}\n'
    )
    with contextlib.redirect_stderr(io.StringIO()):
        parsed = parse_perf_json_stderr(sample_new)
    assert "instructions" in parsed and abs(parsed["instructions"]["value"] - 1234567890) < 1, parsed
    assert "cycles" in parsed and abs(parsed["cycles"]["value"] - 987654321) < 1, parsed

    sample_old = (
        '{"value": 42.0, "event": "task-clock"}\n'
        '{"value": "<not counted>", "event": "branch-misses"}\n'
        'not-json garbage\n'
    )
    with contextlib.redirect_stderr(io.StringIO()):
        parsed_old = parse_perf_json_stderr(sample_old)
    assert "task-clock" in parsed_old and parsed_old["task-clock"]["value"] == 42.0, parsed_old
    assert "branch-misses" not in parsed_old, parsed_old

    # User-mode modifier suffix (paranoid>=2 restricts to userspace).
    sample_modifier = (
        '{"counter-value": "12345", "event": "instructions:u", "stddev": "0"}\n'
        '{"counter-value": "0.45", "unit": "msec", "event": "task-clock:u"}\n'
    )
    with contextlib.redirect_stderr(io.StringIO()):
        parsed_mod = parse_perf_json_stderr(sample_modifier)
    assert "instructions" in parsed_mod and parsed_mod["instructions"]["value"] == 12345, parsed_mod
    assert "task-clock" in parsed_mod, parsed_mod


_selftest_perf_parser()


# ---------------------------------------------------------------------------
# Running configs
# ---------------------------------------------------------------------------

def run_perf(args: list[str]) -> dict[str, dict[str, float]]:
    cmd = [
        "perf", "stat", "-j",
        "-e", ",".join(PERF_EVENTS),
        "-r", str(PERF_REPEATS),
        "--",
        str(BIN_PATH), str(FIXTURE_PATH), *args,
    ]
    # Send riplog stdout to /dev/null so we don't drown the terminal.
    with open(os.devnull, "wb") as devnull:
        res = subprocess.run(
            cmd,
            cwd=REPO_ROOT,
            stdout=devnull,
            stderr=subprocess.PIPE,
            check=False,
        )
    if res.returncode != 0:
        print(
            f"[perf] command failed (exit {res.returncode}): {' '.join(cmd)}",
            file=sys.stderr,
        )
        print(res.stderr.decode("utf-8", errors="replace"), file=sys.stderr)
        sys.exit(4)
    return parse_perf_json_stderr(res.stderr.decode("utf-8", errors="replace"))


def warmup(args: list[str]) -> None:
    """Run the binary once to warm the page cache; output discarded."""
    cmd = [str(BIN_PATH), str(FIXTURE_PATH), *args]
    with open(os.devnull, "wb") as devnull:
        subprocess.run(cmd, cwd=REPO_ROOT, stdout=devnull, stderr=devnull, check=False)


def run_config(name: str, args: list[str]) -> dict[str, float]:
    print(f"[bench] {name}: {' '.join(args) if args else '(no args)'}", flush=True)
    warmup(args)
    perf_out = run_perf(args)
    # Flatten to {event: value}; perf -r already returns the mean.
    return {event: data["value"] for event, data in perf_out.items()}


# ---------------------------------------------------------------------------
# Reporting / baseline I/O
# ---------------------------------------------------------------------------

def load_baseline() -> dict:
    if not BASELINE_PATH.exists():
        return {
            "fixture_sha256": None,
            "perf_event_args": list(PERF_EVENTS),
            "configs": {},
        }
    with BASELINE_PATH.open("r") as f:
        return json.load(f)


def save_baseline(baseline: dict) -> None:
    with BASELINE_PATH.open("w") as f:
        json.dump(baseline, f, indent=2, sort_keys=True)
        f.write("\n")


def format_row(name: str, base: Optional[float], cur: float, threshold: float) -> tuple[str, bool]:
    if base is None or base == 0:
        return (f"  {name:<24} {'(no baseline)':>18} {cur:>18,.0f}     -      NEW", False)
    delta_pct = (cur - base) / base * 100.0
    regressed = delta_pct > threshold
    status = "OK"
    if delta_pct > threshold:
        status = "REGRESS"
    elif delta_pct < -threshold:
        status = "IMPROVE"
    return (
        f"  {name:<24} {base:>18,.0f} {cur:>18,.0f} {delta_pct:+6.2f}%  {status}",
        regressed,
    )


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--list", action="store_true", help="list configs and exit")
    parser.add_argument("--only", metavar="NAME", help="run only this config")
    parser.add_argument(
        "--threshold-pct",
        type=float,
        default=DEFAULT_THRESHOLD_PCT,
        help=f"regression threshold percent (default {DEFAULT_THRESHOLD_PCT})",
    )
    parser.add_argument(
        "--update-baseline",
        action="store_true",
        help="overwrite the baseline JSON with current measurements",
    )
    args = parser.parse_args()

    if args.list:
        for name, cfg_args in CONFIGS:
            print(f"{name}\t{' '.join(cfg_args)}")
        return 0

    check_tools()
    cargo_build()
    check_perf_paranoid()

    baseline = load_baseline()
    sha = ensure_fixture(baseline)

    selected: list[tuple[str, list[str]]]
    if args.only:
        matches = [c for c in CONFIGS if c[0] == args.only]
        if not matches:
            print(f"error: no config named {args.only!r}", file=sys.stderr)
            print("available: " + ", ".join(c[0] for c in CONFIGS), file=sys.stderr)
            return 2
        selected = matches
    else:
        selected = list(CONFIGS)

    results: dict[str, dict[str, float]] = {}
    for name, cfg_args in selected:
        results[name] = run_config(name, cfg_args)

    # Reporting.
    base_configs = baseline.get("configs", {}) or {}
    print()
    print("INSTRUCTIONS (lower is better):")
    print(f"  {'config':<24} {'baseline':>18} {'current':>18} {'delta':>7}  status")
    print(f"  {'-' * 24} {'-' * 18} {'-' * 18} {'-' * 7}  {'-' * 7}")
    any_regress = False
    for name, _ in selected:
        cur_instr = results[name].get("instructions")
        if cur_instr is None:
            print(f"  {name:<24} (no instructions counted)")
            continue
        base_instr = (base_configs.get(name) or {}).get("instructions")
        row, regressed = format_row(name, base_instr, cur_instr, args.threshold_pct)
        print(row)
        if regressed:
            any_regress = True

    # Also report wall clock (informational only).
    print()
    print("WALL CLOCK (task-clock ms, informational):")
    print(f"  {'config':<24} {'baseline':>14} {'current':>14} {'delta':>7}")
    print(f"  {'-' * 24} {'-' * 14} {'-' * 14} {'-' * 7}")
    for name, _ in selected:
        cur_tc = results[name].get("task-clock")
        base_tc = (base_configs.get(name) or {}).get("task-clock")
        if cur_tc is None:
            print(f"  {name:<24}  (no task-clock)")
            continue
        if base_tc is None or base_tc == 0:
            print(f"  {name:<24} {'(no baseline)':>14} {cur_tc:>14,.1f}     -")
        else:
            d = (cur_tc - base_tc) / base_tc * 100.0
            print(f"  {name:<24} {base_tc:>14,.1f} {cur_tc:>14,.1f} {d:+6.2f}%")

    if args.update_baseline:
        baseline["fixture_sha256"] = sha
        baseline["perf_event_args"] = list(PERF_EVENTS)
        new_configs = dict(base_configs)
        for name, _ in selected:
            new_configs[name] = results[name]
        baseline["configs"] = new_configs
        save_baseline(baseline)
        print(f"\n[baseline] wrote {BASELINE_PATH}", flush=True)
        return 0
    else:
        # If the sha was just discovered (first run), persist it.
        if baseline.get("fixture_sha256") == sha and not BASELINE_PATH.exists():
            save_baseline(baseline)

    return 1 if any_regress else 0


if __name__ == "__main__":
    sys.exit(main())
