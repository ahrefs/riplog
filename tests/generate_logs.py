#!/usr/bin/env python3
"""Emit synthetic logfmt lines at a configurable rate.

Each line is `time=<RFC3339> level=<level> msg=<msg>`.
"""

from __future__ import annotations

import argparse
import random
import sys
import time
from datetime import datetime, timedelta, timezone
from typing import Iterator, Optional

LEVELS: tuple[str, ...] = ("debug", "info", "warn", "error")

MESSAGES: tuple[str, ...] = (
    "startup",
    "request handled",
    "cache miss",
    "cache hit",
    "connection reset",
    "user logged in",
    "task complete",
    "retry scheduled",
    "deadline exceeded",
)


def rfc3339(when: datetime) -> str:
    return when.strftime("%Y-%m-%dT%H:%M:%S.") + f"{when.microsecond:06d}Z"


def quote(value: str) -> str:
    if any(c in value for c in ' "='):
        escaped = value.replace("\\", "\\\\").replace('"', '\\"')
        return f'"{escaped}"'
    return value


def line(when: datetime) -> str:
    level = 'critical' if random.random() <= 0.01 else random.choice(LEVELS)
    msg = random.choice(MESSAGES)
    return f"time={rfc3339(when)} level={level} msg={quote(msg)}"


def stream(
    rate: float,
    count: Optional[int],
    start_time: Optional[datetime],
) -> Iterator[str]:
    """Emit lines at `rate` per second.

    When `start_time` is None, timestamps are real-time and the loop sleeps
    to match the rate. When `start_time` is given, timestamps are
    deterministic (start_time + n/rate) and the loop does not sleep — useful
    for generating reproducible test fixtures.
    """
    interval_s = 1.0 / rate
    if start_time is not None:
        emitted = 0
        delta = timedelta(seconds=interval_s)
        when = start_time
        while count is None or emitted < count:
            yield line(when)
            emitted += 1
            when += delta
        return

    next_emit = time.monotonic()
    emitted = 0
    while count is None or emitted < count:
        yield line(datetime.now(timezone.utc))
        emitted += 1
        next_emit += interval_s
        sleep_for = next_emit - time.monotonic()
        if sleep_for > 0:
            time.sleep(sleep_for)
        else:
            # Rate too high to keep up; reset baseline so we don't burst.
            next_emit = time.monotonic()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--rate",
        type=float,
        default=10.0,
        help="lines per second (default: 10)",
    )
    parser.add_argument(
        "--count",
        type=int,
        default=None,
        help="stop after N lines (default: run forever)",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=None,
        help="seed for the RNG",
    )
    parser.add_argument(
        "--start-time",
        type=str,
        default=None,
        help=(
            "RFC 3339 start timestamp (e.g. 2026-04-24T18:00:00Z). When set, "
            "timestamps are deterministic and the generator does not sleep."
        ),
    )
    args = parser.parse_args()

    if args.seed is not None:
        random.seed(args.seed)

    start_time: Optional[datetime] = None
    if args.start_time is not None:
        s = args.start_time.replace("Z", "+00:00")
        start_time = datetime.fromisoformat(s).astimezone(timezone.utc)

    try:
        for entry in stream(args.rate, args.count, start_time):
            print(entry, flush=True)
    except (BrokenPipeError, KeyboardInterrupt):
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
