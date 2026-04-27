use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
pub struct Validate {
    pub file: PathBuf,

    /// Lower bound (RFC 3339 timestamp). When set together with `--to`,
    /// bisects the file to a byte range covering `[from, to]` before
    /// scanning.
    #[arg(long)]
    pub from: Option<String>,

    /// Upper bound (RFC 3339 timestamp).
    #[arg(long)]
    pub to: Option<String>,

    /// Reorder window in seconds. Used to over-approximate the byte range
    /// when bisecting; defaults to 300 (5 minutes reorder at worst).
    #[arg(long, default_value_t = 300)]
    pub window_secs: u64,
}

#[derive(Parser, Debug)]
pub enum Cli {
    Validate(Validate),
}
