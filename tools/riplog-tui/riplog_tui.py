#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "textual>=0.86.2",
# ]
# ///
"""riplog-tui — Textual monitor for riplog aggregate (logfmt) streams.

Reads logfmt lines from stdin and renders, in aggregate mode:
- time-series of count vs bucket.start (one line per group-key combo, top-K),
- top-N horizontal bar histogram of cumulative counts,
- a compact stats panel,
- a small scrolling tail of recent input.

Press `m` to toggle to raw mode: a full-height RichLog of incoming logfmt lines.

Usage:
    riplog -F app.log --count --group-by=svc --bucket=10s | riplog-tui
    tail -F app.log                                       | riplog-tui --raw
"""

from __future__ import annotations

import argparse
import sys
import threading
from collections import deque
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Optional, Union

from rich.markup import escape as escape_markup
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal
from textual.widgets import Footer, RichLog, Static


ComboKey = tuple[tuple[str, str], ...]


# ---------- logfmt parsing ----------

def parse_logfmt(line: str) -> list[tuple[str, str]]:
    """Return ordered (key, value) pairs. Tolerant of malformed input."""
    out: list[tuple[str, str]] = []
    i, n = 0, len(line)
    while i < n:
        while i < n and line[i].isspace():
            i += 1
        if i >= n:
            break
        start = i
        while i < n and line[i] != "=" and not line[i].isspace():
            i += 1
        key = line[start:i]
        if i >= n or line[i] != "=":
            continue
        i += 1
        if i < n and line[i] == '"':
            i += 1
            buf: list[str] = []
            while i < n and line[i] != '"':
                if line[i] == "\\" and i + 1 < n:
                    buf.append(line[i + 1])
                    i += 2
                else:
                    buf.append(line[i])
                    i += 1
            if i < n:
                i += 1
            value = "".join(buf)
        else:
            start = i
            while i < n and not line[i].isspace():
                i += 1
            value = line[start:i]
        if key:
            out.append((key, value))
    return out


# ---------- events ----------

@dataclass
class Aggregate:
    count: int
    combo: ComboKey
    bucket_start: Optional[datetime]
    time_start: Optional[datetime]
    time_end: Optional[datetime]


@dataclass
class Raw:
    line: str
    kvs: list[tuple[str, str]]


Event = Union[Aggregate, Raw]


def _parse_ts(s: str) -> Optional[datetime]:
    try:
        return datetime.fromisoformat(s)
    except ValueError:
        return None


def classify(line: str) -> Event:
    kvs = parse_logfmt(line)
    if kvs and kvs[0][0] == "count":
        try:
            count = int(kvs[0][1])
        except ValueError:
            return Raw(line, kvs)
        combo: list[tuple[str, str]] = []
        bucket_start = time_start = time_end = None
        for k, v in kvs[1:]:
            if k.startswith("key."):
                combo.append((k[4:], v))
            elif k == "bucket.start":
                bucket_start = _parse_ts(v)
            elif k == "time.start":
                time_start = _parse_ts(v)
            elif k == "time.end":
                time_end = _parse_ts(v)
        combo.sort()
        return Aggregate(count, tuple(combo), bucket_start, time_start, time_end)
    return Raw(line, kvs)


# ---------- model ----------

def humanize_duration(secs: float) -> str:
    if secs < 1:
        return "<1s"
    if secs < 60:
        return f"{secs:.0f}s"
    m, s = divmod(int(secs), 60)
    if m < 60:
        return f"{m}m {s:02d}s"
    h, m = divmod(m, 60)
    return f"{h}h {m:02d}m"


def combo_label(combo: ComboKey, max_len: int = 24) -> str:
    if not combo:
        return "all"
    s = "|".join(f"{k}={v}" for k, v in combo)
    return s if len(s) <= max_len else s[: max_len - 1] + "…"


_SPARK_BLOCKS = " ▁▂▃▄▅▆▇█"


def sparkline(values: list[int], width: int) -> str:
    """Tail-truncate `values` to `width` and render as block-character sparkline."""
    if width < 1 or not values:
        return ""
    vs = values[-width:]
    vmax = max(vs) or 1
    return "".join(_SPARK_BLOCKS[min(8, round(v / vmax * 8))] for v in vs)


def hbar(value: int, vmax: int, width: int) -> str:
    """Block-character horizontal bar of length proportional to `value`."""
    if width < 1 or vmax <= 0:
        return ""
    full = value / vmax * width
    whole = int(full)
    frac = full - whole
    bar = "█" * whole
    if whole < width:
        # Partial cell uses ▏▎▍▌▋▊▉ for finer resolution.
        eighths = " ▏▎▍▌▋▊▉█"
        bar += eighths[min(8, round(frac * 8))]
    return bar


class Model:
    def __init__(self, max_lines: int = 500, top_series: int = 8) -> None:
        self.series: dict[ComboKey, list[tuple[datetime, int]]] = {}
        self.totals: dict[ComboKey, int] = {}
        self.bucket_starts: set[datetime] = set()
        self.total_count: int = 0
        self.first_ts: Optional[datetime] = None
        self.last_ts: Optional[datetime] = None
        self.raw_lines_count: int = 0
        self.raw_levels: set[str] = set()
        self.recent: deque[str] = deque(maxlen=max_lines)
        self.top_series = top_series
        self.paused = False
        self.start_wall = datetime.now(timezone.utc)

    def ingest_line(self, line: str) -> Event:
        line = line.rstrip("\n").rstrip("\r")
        evt = classify(line) if line else Raw(line, [])
        if self.paused:
            return evt
        self.recent.append(line)
        if isinstance(evt, Aggregate):
            self._ingest_aggregate(evt)
        else:
            self._ingest_raw(evt)
        return evt

    def _ingest_aggregate(self, evt: Aggregate) -> None:
        self.total_count += evt.count
        ts = evt.bucket_start or evt.time_start
        if ts is not None:
            if self.first_ts is None or ts < self.first_ts:
                self.first_ts = ts
            if self.last_ts is None or ts > self.last_ts:
                self.last_ts = ts
        if evt.bucket_start is not None:
            self.bucket_starts.add(evt.bucket_start)
            self.series.setdefault(evt.combo, []).append((evt.bucket_start, evt.count))
        self.totals[evt.combo] = self.totals.get(evt.combo, 0) + evt.count

    def _ingest_raw(self, evt: Raw) -> None:
        self.raw_lines_count += 1
        for k, v in evt.kvs:
            if k == "level":
                self.raw_levels.add(v)
                break

    def top_combos(self, k: int) -> list[ComboKey]:
        items = sorted(self.totals.items(), key=lambda kv: (-kv[1], kv[0]))
        return [c for c, _ in items[:k]]

    def has_time_series(self) -> bool:
        return len(self.bucket_starts) >= 2

    def aggregate_stats(self) -> str:
        rate = 0.0
        span = "—"
        if self.first_ts and self.last_ts and self.last_ts > self.first_ts:
            secs = (self.last_ts - self.first_ts).total_seconds()
            span = humanize_duration(secs)
            if secs > 0:
                rate = self.total_count / secs
        suffix = "  [PAUSED]" if self.paused else ""
        return (
            f"total={self.total_count}  rate={rate:.1f}/s  "
            f"combos={len(self.totals)}  buckets={len(self.bucket_starts)}  "
            f"span={span}{suffix}"
        )

    def raw_stats(self) -> str:
        secs = (datetime.now(timezone.utc) - self.start_wall).total_seconds()
        rate = self.raw_lines_count / secs if secs >= 2 else 0.0
        suffix = "  [PAUSED]" if self.paused else ""
        return (
            f"lines={self.raw_lines_count}  rate={rate:.1f}/s  "
            f"levels={len(self.raw_levels)}  span={humanize_duration(secs)}{suffix}"
        )


# ---------- app ----------

class RiplogTUI(App):
    CSS = """
    #stats {
        dock: top;
        height: 1;
        padding: 0 1;
        background: $boost;
        color: $text;
    }
    #aggregate-view { layout: vertical; height: 1fr; }
    #timeseries { height: 60%; padding: 0 1; }
    #bottom-row { height: 40%; layout: horizontal; }
    #topn { width: 60%; padding: 0 1; }
    #tail { width: 40%; border-left: solid $accent; }
    #raw-view { layout: vertical; height: 1fr; }
    #raw-log { height: 1fr; }
    """

    BINDINGS = [
        Binding("m", "toggle_mode", "Mode"),
        Binding("p", "toggle_pause", "Pause"),
        Binding("ctrl+l", "clear", "Clear"),
        Binding("q", "quit", "Quit"),
    ]

    def __init__(
        self,
        mode: str = "aggregate",
        max_lines: int = 500,
        top_series: int = 8,
    ) -> None:
        super().__init__()
        self.model = Model(max_lines=max_lines, top_series=top_series)
        self.mode = mode

    def compose(self) -> ComposeResult:
        yield Static("waiting for input…", id="stats")
        with Container(id="aggregate-view"):
            yield Static("", id="timeseries")
            with Horizontal(id="bottom-row"):
                yield Static("", id="topn")
                yield RichLog(id="tail", max_lines=2000, markup=False, highlight=False)
        with Container(id="raw-view"):
            yield RichLog(id="raw-log", max_lines=5000, markup=False, highlight=False)
        yield Footer()

    def on_mount(self) -> None:
        self._apply_mode()
        self._chart_dirty = False
        self._seen_data = False
        self.set_interval(0.5, self.refresh_views)
        self._start_stdin_thread()

    def _apply_mode(self) -> None:
        self.query_one("#aggregate-view", Container).display = self.mode == "aggregate"
        self.query_one("#raw-view", Container).display = self.mode == "raw"

    # --- ingest ---

    def _start_stdin_thread(self) -> None:
        def reader() -> None:
            try:
                for line in sys.stdin:
                    self.call_from_thread(self._ingest, line)
            except Exception:
                pass

        threading.Thread(target=reader, daemon=True).start()

    def _ingest(self, line: str) -> None:
        if self.model.paused:
            return
        evt = self.model.ingest_line(line)
        text = line.rstrip("\n").rstrip("\r")
        if not text:
            return
        if isinstance(evt, Aggregate):
            self._chart_dirty = True
        if self.mode == "aggregate":
            self.query_one("#tail", RichLog).write(text)
        else:
            self.query_one("#raw-log", RichLog).write(text)

    # --- redraw ---

    def refresh_views(self) -> None:
        stats = self.query_one("#stats", Static)
        if self.mode == "aggregate":
            stats.update(self.model.aggregate_stats())
            if self._chart_dirty:
                if self.model.totals:
                    self.query_one("#timeseries", Static).update(self._render_timeseries())
                    self.query_one("#topn", Static).update(self._render_topn())
                    self._seen_data = True
                elif not self._seen_data:
                    self.query_one("#timeseries", Static).update("[dim]time-series · waiting for data…[/]")
                    self.query_one("#topn", Static).update("[dim]top-N · no data yet[/]")
                # If totals is empty but we've seen data before, keep the
                # last rendered chart on screen — better stale than blank.
                self._chart_dirty = False
        else:
            stats.update(self.model.raw_stats())

    def _render_timeseries(self) -> str:
        if not self.model.bucket_starts:
            return "[dim]time-series · waiting for data…[/]"
        top = self.model.top_combos(self.model.top_series)
        if not top:
            return "[dim]time-series · waiting for data…[/]"
        widget = self.query_one("#timeseries", Static)
        total_w = max(20, widget.size.width - 2)
        label_w = min(28, max(len(combo_label(c, 24)) for c in top))
        # label + 2 spaces + spark + 2 spaces + count(7)
        spark_w = max(8, total_w - label_w - 11)
        lines = [f"[b]count over time[/b]  [dim]({len(top)} of {len(self.model.totals)} series, last {spark_w} buckets)[/]"]
        for combo in top:
            pts = self.model.series.get(combo, [])
            ys = [p[1] for p in pts]
            if not ys:
                continue
            label = escape_markup(combo_label(combo, label_w)).ljust(label_w)
            spark = sparkline(ys, width=spark_w)
            last = ys[-1]
            lines.append(f"{label}  {spark}  [b]{last:>5}[/]")
        return "\n".join(lines)

    def _render_topn(self) -> str:
        items = sorted(self.model.totals.items(), key=lambda kv: (-kv[1], kv[0]))[:12]
        if not items:
            return "[dim]top-N · no data yet[/]"
        widget = self.query_one("#topn", Static)
        total_w = max(20, widget.size.width - 2)
        label_w = min(28, max(len(combo_label(c, 24)) for c, _ in items))
        # label + 2 spaces + bar + 1 space + count(8)
        bar_w_max = max(4, total_w - label_w - 12)
        vmax = max(n for _, n in items)
        lines = [f"[b]top-N by total count[/b]"]
        for combo, n in items:
            label = escape_markup(combo_label(combo, label_w)).ljust(label_w)
            bar = hbar(n, vmax, bar_w_max)
            lines.append(f"{label}  {bar} [b]{n}[/]")
        return "\n".join(lines)

    # --- actions ---

    def action_toggle_mode(self) -> None:
        self.mode = "raw" if self.mode == "aggregate" else "aggregate"
        self._apply_mode()
        self._chart_dirty = True
        self.refresh_views()

    def action_toggle_pause(self) -> None:
        self.model.paused = not self.model.paused

    def action_clear(self) -> None:
        # Audit trail: if data ever vanishes unexpectedly, we want to know
        # whether this was triggered.
        try:
            with open("/tmp/riplog-tui.log", "a") as f:
                f.write(f"{datetime.now().isoformat()} action_clear fired\n")
        except OSError:
            pass
        cap = self.model.recent.maxlen or 500
        self.model = Model(max_lines=cap, top_series=self.model.top_series)
        self.query_one("#tail", RichLog).clear()
        self.query_one("#raw-log", RichLog).clear()
        self._chart_dirty = True
        self._seen_data = False
        self.refresh_views()


def main() -> None:
    p = argparse.ArgumentParser(prog="riplog-tui", description=__doc__)
    p.add_argument("--raw", action="store_true", help="start in raw logfmt mode")
    p.add_argument("--max-lines", type=int, default=500, help="tail buffer cap")
    p.add_argument("--top-series", type=int, default=8, help="max series in time-series chart")
    args = p.parse_args()

    if sys.stdin.isatty():
        print("riplog-tui: refusing to run with no piped stdin", file=sys.stderr)
        print(
            "usage: <riplog --count ... | riplog-tui> "
            "or <tail -F file | riplog-tui --raw>",
            file=sys.stderr,
        )
        sys.exit(2)

    RiplogTUI(
        mode="raw" if args.raw else "aggregate",
        max_lines=args.max_lines,
        top_series=args.top_series,
    ).run()


if __name__ == "__main__":
    main()
