use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Parser, Debug)]
#[command(about = "Slice and filter logfmt streams.", term_width = 80)]
pub struct Cli {
    /// Input file(s). If omitted, reads from stdin. Multiple files are
    /// processed in order; aggregated output (`--count`, `--group-by`,
    /// `--list-keys`, `--list-values-for`) is emitted once at the end and
    /// reflects the union of all files.
    /// `-` is accepted as a file argument meaning stdin (may appear at most
    /// once), processed in position alongside files — e.g.
    /// `riplog rotated.log.1 rotated.log -` streams the two rotated logs,
    /// then stdin. Symbolic `--from`/`--to` anchors and `--n-buckets`
    /// resolve against the real files' span (stdin is skipped); the
    /// resolved window is then applied as a per-line filter to stdin too.
    /// `-` cannot be combined with `-f`, `-F`, or `--time-range`.
    /// Note: `--from`/`--to` are applied per file (each file is bisected
    /// independently, then strictly time-filtered), so a time range that
    /// straddles a log rotation works as expected. `-f`/`-F` is attached
    /// only to the *last* file — e.g. `riplog foo.log.1 foo.log -F` reads
    /// the rotated log, then the current log, then keeps tailing it.
    /// `--time-range` reports the span across all given files (min of
    /// per-file firsts, max of per-file lasts). Stdin (no file) is not
    /// seekable, so none of `-f`, `-F`, `--from`, `--to`, `--time-range`
    /// work without a file argument.
    pub files: Vec<PathBuf>,

    /// Lower bound. Forms: full RFC 3339 (`2026-04-24T18:09:03Z`),
    /// date-only (`2026-04-24`), time-of-day (`18:00`, anchored to the
    /// file's first timestamp), or symbolic (`start`, `start+1h`,
    /// `end-30m`). Duration units accept word and plural forms with optional
    /// whitespace, e.g. `start+5 min`, `end-2 days`, `start+1 hour`.
    #[arg(long)]
    pub from: Option<String>,

    /// Upper bound. Same forms as `--from`; time-of-day anchors to the
    /// file's last timestamp.
    #[arg(long)]
    pub to: Option<String>,

    /// Reorder window in seconds, used to over-approximate the bisected byte
    /// range. Defaults to 10s.
    #[arg(long, default_value_t = 10)]
    pub window_secs: u64,

    /// Filter expression. Combine leaf predicates `<key> <op> <value>`
    /// (`=`, `!=`, `<`, `<=`, `>`, `>=`, `=~`) and existence checks
    /// `exists <key>` with `and`, `or`, `not`, and parentheses. Repeatable;
    /// multiple `--if` flags are AND-ed.
    /// Examples: `--if 'level>=warn'`,
    /// `--if 'level=error and (facil=net or facil=db)'`,
    /// `--if 'not msg =~ "noisy.*timeout"'`,
    /// `--if 'exists trace_id and level>=warn'`.
    /// `--where` is an alias.
    #[arg(long = "if", visible_alias = "where")]
    pub keys: Vec<String>,

    /// Follow the file: bisect to `--from` (if set) or start at EOF, stream
    /// matching lines, then keep reading new lines as they're appended.
    /// Like `tail -f`: stays on the same inode; if the file is rotated or
    /// truncated, no further lines will be read.
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Like `-f`, but reopen the file on rotation (inode change) or
    /// truncation, like `tail -F`.
    #[arg(short = 'F', long = "follow-reopen")]
    pub follow_reopen: bool,

    /// Write output to FILE instead of stdout.
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// Group matched lines by the value of one or more keys (requires
    /// `--count`). Emits one logfmt row per combination at the end:
    /// `count=N <group keys> [bucket=...] ts_start=... ts_end=...`.
    /// Repeatable, and a single flag may carry a comma-separated list —
    /// `--group-by level,facil` and `--group-by level --group-by facil`
    /// both produce one row per (level, facil) combination. `ts_start` and
    /// `ts_end` are the min/max timestamps observed in the group; they are
    /// omitted for groups with no parseable timestamp.
    #[arg(long = "group-by", value_delimiter = ',', requires = "count")]
    pub group_by: Vec<String>,

    /// Add a fixed-width, epoch-aligned time bucket dimension to the
    /// grouping. Same duration syntax as `--from start+<dur>` (`5m`, `30s`,
    /// `2 hours`). Under `-f`/`-F`, or when reading from stdin, switches to
    /// streaming output: rows emit as buckets close (when `max_ts_seen >
    /// bucket.end + window_secs`). Requires `--count`. Conflicts with
    /// `--n-buckets`.
    #[arg(
        long,
        value_name = "DURATION",
        requires = "count",
        conflicts_with = "n_buckets"
    )]
    pub bucket: Option<String>,

    /// Divide the active time range into `N` equal-width buckets aligned to
    /// the window start. Window comes from `--from`/`--to` when set, else
    /// from the file's first/last timestamps. Requires `--count` and a file
    /// argument. Conflicts with `--bucket`.
    #[arg(long = "n-buckets", value_name = "N", requires = "count")]
    pub n_buckets: Option<usize>,

    /// Suppress line output; instead, gather every distinct key seen on
    /// matched lines and print the sorted list at the end.
    #[arg(long = "list-keys")]
    pub list_keys: bool,

    /// Suppress line output; gather every distinct value seen for the given
    /// key on matched lines. Repeatable, and a single flag may carry a
    /// comma-separated list — `--list-values-for level,facil` and
    /// `--list-values-for level --list-values-for facil` are equivalent.
    #[arg(long = "list-values-for", value_delimiter = ',')]
    pub list_values_for: Vec<String>,

    /// Suppress line output; for each matched line, emit only the unquoted,
    /// unescaped value of `<key>` (one per line). Lines without the key are
    /// dropped. Useful for piping a single field downstream, e.g.
    /// `riplog app.log --if 'level=error' --raw-key msg | sort | uniq -c`.
    #[arg(long = "raw-key", value_name = "KEY")]
    pub raw_key: Option<String>,

    /// Suppress line output; print only the count of matched lines at the
    /// end. Combine with `-F` and Ctrl-C to count live.
    #[arg(long)]
    pub count: bool,

    /// Stop after this many matched lines.
    #[arg(short = 'n', long)]
    pub limit: Option<usize>,

    /// Randomly drop matched lines, keeping each with probability `rate`
    /// (a float in `[0, 1]`). Applied after `--if` and time-window filters,
    /// before counters/sinks — so `--count`, `--count-by`, etc. reflect the
    /// post-sampling set. Combine with `--sample-if` to scope the sampling.
    #[arg(long = "sample-rate", value_name = "RATE")]
    pub sample_rate: Option<f64>,

    /// Restrict `--sample-rate` to lines matching this expression. Same
    /// syntax as `--if`. Lines that don't match `--sample-if` are kept
    /// unconditionally; matching lines are sampled at `--sample-rate`.
    /// Useful for downsampling chatty subsets without thinning the rest.
    #[arg(long = "sample-if", value_name = "EXPR")]
    pub sample_if: Option<String>,

    /// Print the first and last timestamps in the file. Scans the head and
    /// tail (≈1 MiB each) and returns the min/max so slight reordering at
    /// the edges doesn't skew the result. Suppresses normal output.
    #[arg(long = "time-range")]
    pub time_range: bool,

    /// Timezone for displayed timestamps. Accepts `utc`, `local`, an IANA
    /// name like `Europe/Paris`, or a fixed offset like `+02:00`.
    /// Defaults to `utc`. Only affects display; parsing of input timestamps
    /// is unchanged.
    #[arg(long)]
    pub tz: Option<String>,

    /// Colorize output. `auto` (default) enables when stdout is a terminal
    /// and `-o` is not used.
    #[arg(long, value_enum, default_value_t = ColorMode::Auto, env="COLOR")]
    pub color: ColorMode,

    /// Emit output as JSONL (one JSON value per line) instead of logfmt.
    /// Each matched line becomes a JSON object; aggregation rows become
    /// JSON objects with flat keys (`count`, `key.<k>`, `bucket.start`, …);
    /// `--list-keys` / `--list-values-for` emit a single JSON array.
    /// Conflicts with `--raw-key` and `--color=always`.
    #[arg(long, conflicts_with = "raw_key")]
    pub json: bool,

    /// Buffer matched lines and emit them at end sorted by `<KEY>`'s value
    /// (lexicographic). Lines lacking the key sort first (as if their value
    /// were the empty string). **Holds every matched line in memory** —
    /// narrow with `--if` first if the input is large. Composes with
    /// `--raw-key` (the emitted unescaped values are sorted by `<KEY>`).
    #[arg(long = "sort-by", value_name = "KEY")]
    pub sort_by: Option<String>,

    /// Append `key=value` pairs at the end of each emitted line (after any
    /// `--rm`). Repeatable; each occurrence may be comma-separated (same
    /// rule as `--group-by`). Values cannot contain commas; use multiple
    /// `--add` flags instead.
    #[arg(long = "add", value_name = "KEY=VALUE", value_delimiter = ',')]
    pub add: Vec<String>,

    /// Drop every pair with this key before output. Repeatable;
    /// comma-separated lists are split like `--group-by`. Must not name a key
    /// used by `--group-by`, `--sort-by`, `--list-values-for`, or `--raw-key`.
    #[arg(long = "rm", value_name = "KEY", value_delimiter = ',')]
    pub rm: Vec<String>,

    /// Search a single file in parallel. `-j` (no value) uses every available
    /// core; `-j=4` uses 4 worker threads. Output is **unordered** — workers
    /// emit matched lines as they go. Pipe through `--sort-by` or external
    /// `sort` if you need a deterministic order. Falls back to sequential for
    /// stdin, follow mode (`-f`/`-F`), and small inputs. Cannot be combined
    /// with `-n`/`--limit`.
    #[arg(short = 'j', long = "parallel", value_name = "N",
          num_args = 0..=1, default_missing_value = "0",
          require_equals = true)]
    pub parallel: Option<usize>,
}

impl Cli {
    /// `--window-secs` as i64 nanoseconds. Used both as the bisect overshoot
    /// in `plan_file` and as the bucket-close grace in streaming-bucket mode.
    pub fn window_nanos(&self) -> i64 {
        (self.window_secs as i64).saturating_mul(1_000_000_000)
    }
}
