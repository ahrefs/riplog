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
#[command(about = "Slice and filter logfmt streams.")]
pub struct Cli {
    /// Input file. If omitted, reads from stdin.
    /// Note: `-F`, `--from`, `--to` require a real file (stdin is not seekable).
    pub file: Option<PathBuf>,

    /// Lower bound. Forms: full RFC 3339 (`2026-04-24T18:09:03Z`),
    /// date-only (`2026-04-24`), time-of-day (`18:00`, anchored to the
    /// file's first timestamp), or symbolic (`start`, `start+1h`, `end-30m`).
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
    /// (`=`, `!=`, `<`, `<=`, `>`, `>=`, `=~`) with `and`, `or`, `not`,
    /// and parentheses. Repeatable; multiple `--if` flags are AND-ed.
    /// Examples: `--if 'level>=warn'`,
    /// `--if 'level=error and (facil=net or facil=db)'`,
    /// `--if 'not msg =~ "noisy.*timeout"'`.
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
    #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
    pub color: ColorMode,
}
