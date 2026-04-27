use std::path::PathBuf;

use clap::Parser;

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
    /// range. Defaults to 5 minutes.
    #[arg(long, default_value_t = 300)]
    pub window_secs: u64,

    /// Repeated key/value filter. Operators: `=`, `!=`, `<`, `<=`, `>`,
    /// `>=`, `=~`. Examples: `--key level=error`, `--key dur>=100`,
    /// `--key msg=~"connection.*reset"`. Multiple `--key` flags are AND-ed.
    #[arg(long = "key")]
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

    /// Suppress line output; print only the count of matched lines at the
    /// end. Combine with `-F` and Ctrl-C to count live.
    #[arg(long)]
    pub count: bool,
}
