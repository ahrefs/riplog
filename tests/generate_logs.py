#!/usr/bin/env python3
"""Emit synthetic logfmt lines at a configurable rate.

Each line is `time=<RFC3339> level=<level> msg=<msg>`.
"""

from __future__ import annotations

import argparse
import random
import sys
import time
from datetime import datetime, timezone
from typing import Iterator

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


def rfc3339_now() -> str:
    now = datetime.now(timezone.utc)
    return now.strftime("%Y-%m-%dT%H:%M:%S.") + f"{now.microsecond:06d}Z"


def quote(value: str) -> str:
    if any(c in value for c in ' "='):
        escaped = value.replace("\\", "\\\\").replace('"', '\\"')
        return f'"{escaped}"'
    return value


def line() -> str:
    level = random.choice(LEVELS)
    msg = random.choice(MESSAGES)
    return f"time={rfc3339_now()} level={level} msg={quote(msg)}"


def stream(rate: float, count: int | None) -> Iterator[str]:
    interval = 1.0 / rate
    next_emit = time.monotonic()
    emitted = 0
    while count is None or emitted < count:
        yield line()
        emitted += 1
        next_emit += interval
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
    args = parser.parse_args()

    if args.seed is not None:
        random.seed(args.seed)

    try:
        for entry in stream(args.rate, args.count):
            print(entry, flush=True)
    except (BrokenPipeError, KeyboardInterrupt):
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
