# riplog

Slice and filter logfmt streams. Reads one or more files (or stdin),
bisects on timestamp when given a time window, and applies boolean
filters per line.

```
riplog [OPTIONS] [FILE]...
```

Multiple files are processed in order; aggregated output (`--count`,
`--group-by`, `--list-keys`, `--list-values-for`) is emitted once at
the end and reflects the union of all files. `--from`/`--to` are
applied per file (each file is bisected independently), so a time
range that straddles a log rotation works as expected. `-f`/`-F` is
attached to the *last* file — `riplog foo.log.1 foo.log -F` reads the
rotated log, then the current log, then keeps tailing it.

Use `-` as a file argument to read from stdin in position — e.g.
`riplog rotated.log.1 rotated.log - --if 'level=error'` streams the
two rotated logs, then stdin. `-` may appear at most once, and is
incompatible with `-f`, `-F`, and `--time-range`. With `-` in the
list, symbolic `--from`/`--to` anchors and `--n-buckets` resolve
against the real files' span; the resolved window is then applied
as a per-line filter to stdin too. `--bucket=DURATION` switches to
streaming output (per-bucket rows emit as they close).

## Install

Clone this and `cargo install --path=.` from inside the repo should do it.
Make sure `~/.cargo/bin` is in your path.

## Time slicing

- `--from <T>`, `--to <T>`: bounds. Forms: RFC 3339
  (`2026-04-24T18:09:03Z`), date (`2026-04-24`), time-of-day (`18:00`,
  anchored to the file's first/last timestamp), or symbolic (`start`,
  `start+1h`, `end-30m`). Duration units accept word and plural forms
  with optional whitespace: `start+5 min`, `end-2 days`,
  `start+1 hour`, `end - 30 seconds`. Units: `s/sec/seconds`,
  `m/min/minutes`, `h/hour/hours`, `d/day/days`.
- `--window-secs <N>`: reorder tolerance for the bisect. Default `10`.
- `--time-range`: print the first and last timestamps. With multiple
  files, reports the span across all of them (min of per-file firsts,
  max of per-file lasts).
- `--tz <Z>`: display timezone. `utc`, `local`, IANA name, or `+02:00`.

## Filtering

- `--if <EXPR>` (alias `--where`): boolean expression. Repeatable;
  multiple `--if` are AND-ed.
  - leaf predicates: `<key> <op> <value>` with op in
    `=`, `!=`, `<`, `<=`, `>`, `>=`, `=~`
  - existence: `exists <key>`
  - logical: `and`, `or`, `not`, parentheses
  - examples: `level>=warn`,
    `level=error and (facil=net or facil=db)`,
    `not msg =~ "noisy.*timeout"`,
    `exists trace_id and level>=warn`

## Sampling

- `--sample-rate <RATE>`: keep each matched line with probability `RATE`
  in `[0, 1]`. Applied after `--if` / time filters; counters reflect the
  post-sampling set.
- `--sample-if <EXPR>`: restrict the dice roll to lines matching this
  expression (same syntax as `--if`); other lines pass through.

## Output

- `-o, --output <FILE>`: write to file instead of stdout.
- `--color <auto|always|never>`: colorize. `auto` is on for terminal
  stdout without `-o`.
- `-n, --limit <N>`: stop after `N` matched lines.
- `--json`: emit JSONL (one JSON value per line) instead of logfmt.
  Matched lines become JSON objects with keys in logfmt parse order;
  all values stay JSON strings (no number/bool coercion). Aggregation
  rows (`--count` + `--group-by`/`--bucket`/`--n-buckets`) become JSON
  objects with flat keys (`count` is a number, `key.<k>`, `bucket.start`,
  `bucket.end`, `time.start`, `time.end` are strings). `--count` alone
  emits `{"count": N}`. `--list-keys` and `--list-values-for` emit one
  JSON array (multi-key `--list-values-for` emits an array of
  `{"key": K, "value": V}` objects). Conflicts with `--raw-key` and
  `--color=always`.

## Aggregation (suppresses line output)

- `--count`: print the matched-line count. Alone, emits a bare number.
  Combined with `--group-by`, `--bucket`, and/or `--n-buckets`, emits
  one logfmt row per group instead.
- `--group-by <KEY[,KEY…]>`: group matched lines by key value (requires
  `--count`). One logfmt row per tuple of values:
  `count=N key.<k1>=… key.<k2>=… time.start=… time.end=…`. User keys
  are prefixed with `key.` to disambiguate from synthetic columns;
  `time.start`/`time.end` are the min/max observed timestamps in the
  group. Repeatable, and a single flag may carry a comma-separated
  list — `--group-by level,facil` and
  `--group-by level --group-by facil` are equivalent.
- `--bucket <DURATION>`: with `--count`, partition the time range into
  fixed-size, epoch-aligned buckets; each row carries
  `bucket.start=<RFC 3339>` and `bucket.end=<RFC 3339>` boundaries.
  Same duration syntax as `--from start+<dur>`: `5m`, `30s`, `2 hours`.
  Lines without a parseable timestamp are dropped (they can't be
  bucketed). Composes with `--group-by` to bucket per
  (group keys, time slice). Mutually exclusive with `--n-buckets`.
- `--n-buckets <N>`: with `--count`, divide the active time window into
  `N` equal-width buckets aligned to the window start. Width is
  `(end − start) / N`, taking `start`/`end` from `--from`/`--to`
  when set, otherwise from the file's first/last timestamps. Useful
  for graphing — gives a fixed number of points across the time range.
  Requires a file argument. Mutually exclusive with `--bucket`.
- `--list-keys`: print every distinct key seen on matched lines.
- `--list-values-for <KEY[,KEY…]>`: print every distinct value seen for
  `KEY`. Repeatable / comma-separated like `--group-by`.
- `--raw-key <KEY>`: emit only the unquoted, unescaped value of `KEY`
  for each matched line (one per line; lines without the key are
  skipped). Handy for piping a single field downstream, e.g.
  `riplog app.log --if 'level=error' --raw-key msg | sort | uniq -c`.

Example — error rate per service in 5-minute slices:

```
riplog --count --group-by=facil --bucket=5m --if 'level>=error' \
       --from start --to end app.log
```

emits rows like:

```
count=42 key.facil=db bucket.start=2026-05-06T12:00:00Z bucket.end=2026-05-06T12:05:00Z time.start=2026-05-06T12:00:01Z time.end=2026-05-06T12:04:58Z
count=17 key.facil=net bucket.start=2026-05-06T12:05:00Z bucket.end=2026-05-06T12:10:00Z time.start=2026-05-06T12:05:03Z time.end=2026-05-06T12:09:51Z
```

For graphing, `--n-buckets` is often more convenient than `--bucket`:

```
riplog --count --n-buckets=60 --if 'level>=error' --from start --to end app.log
```

always emits 60 rows — one per equal-width slice across the range.

### Streaming under `-f` / `-F` (and stdin)

When `--bucket=DURATION` is combined with `-f`, `-F`, or stdin input,
output switches to streaming mode: each bucket's row is emitted as soon as
the bucket closes, in time-ascending order. A bucket B closes once
`max_ts_seen > B.end + window_secs` (the existing reorder-tolerance flag
doubles as the close grace). On `Ctrl-C` / EOF, any still-open buckets are
flushed in time order. This makes
`riplog -F app.log --count --group-by=svc --bucket=1m` a live histogram
pipe suitable for graph tooling, and lets stdin act as a one-shot pipe:
`gen | riplog --count --group-by=svc --bucket=10s | dashboard`.

`-f` / `-F` rejects two combinations: `--n-buckets` (the upper bound is
unknown in follow mode) and `--group-by` without `--bucket` (no completion
signal — nothing would print until you `Ctrl-C`).

## Follow

- `-f, --follow`: like `tail -f`; stays on the same inode.
- `-F, --follow-reopen`: like `tail -F`; reopens on rotation/truncation.
