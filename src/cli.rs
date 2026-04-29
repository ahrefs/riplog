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
    /// processed in order; aggregated output (`--count`, `--count-by`,
    /// `--list-keys`, `--list-values-for`) is emitted once at the end and
    /// reflects the union of all files.
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

    /// Group matched lines by the value of one or more keys; print a count
    /// table at the end. Repeatable: `--count-by level --count-by facil`
    /// produces one row per (level, facil) combination.
    #[arg(long = "count-by")]
    pub count_by: Vec<String>,

    /// Suppress line output; instead, gather every distinct key seen on
    /// matched lines and print the sorted list at the end.
    #[arg(long = "list-keys")]
    pub list_keys: bool,

    /// Suppress line output; gather every distinct value seen for the given
    /// key on matched lines. Repeatable.
    #[arg(long = "list-values-for")]
    pub list_values_for: Vec<String>,

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
}
