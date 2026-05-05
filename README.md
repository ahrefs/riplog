# riplog

Slice and filter logfmt streams. Reads one or more files (or stdin),
bisects on timestamp when given a time window, and applies boolean
filters per line.

```
riplog [OPTIONS] [FILE]...
```

Multiple files are processed in order; aggregated output (`--count`,
`--count-by`, `--list-keys`, `--list-values-for`) is emitted once at the
end and reflects the union of all files. `--from`/`--to` are applied
per file (each file is bisected independently), so a time range that
straddles a log rotation works as expected. `-f`/`-F` is attached to
the *last* file — `riplog foo.log.1 foo.log -F` reads the rotated log,
then the current log, then keeps tailing it.

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

## Aggregation (suppresses line output)

- `--count`: print the matched-line count.
- `--count-by <KEY[,KEY…]>`: group matched lines by key value, print a
  count table. Repeatable, and a single flag may carry a
  comma-separated list — `--count-by level,facil` and
  `--count-by level --count-by facil` are equivalent. One row per tuple
  of values.
- `--list-keys`: print every distinct key seen on matched lines.
- `--list-values-for <KEY[,KEY…]>`: print every distinct value seen for
  `KEY`. Repeatable / comma-separated like `--count-by`.
- `--raw-key <KEY>`: emit only the unquoted, unescaped value of `KEY`
  for each matched line (one per line; lines without the key are
  skipped). Handy for piping a single field downstream, e.g.
  `riplog app.log --if 'level=error' --raw-key msg | sort | uniq -c`.

## Follow

- `-f, --follow`: like `tail -f`; stays on the same inode.
- `-F, --follow-reopen`: like `tail -F`; reopens on rotation/truncation.
