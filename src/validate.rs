use humanize_bytes::humanize_bytes_binary;
use std::{io::BufRead, time::Instant};

use crate::logfmt;

/// Validate that this is a proper logfmt file
pub fn validate(v: &crate::cli::Validate) -> anyhow::Result<()> {
    let file = std::fs::File::open(&v.file)?;
    let mut reader = std::io::BufReader::new(file);

    let mut line_buf = Vec::new();
    let mut total_read: usize = 0;
    let mut invalid_utf: usize = 0;
    let mut total_pairs: usize = 0;
    let mut total_overflow: usize = 0;

    let start = Instant::now();

    loop {
        let n = match reader.read_until(b'\n', &mut line_buf) {
            Ok(n) => n,
            Err(err) => {
                log::debug!("failed after {total_read} bytes: {err}");
                return Err(err.into());
            }
        };

        if n == 0 {
            break;
        }

        let line = match str::from_utf8(&line_buf) {
            Ok(s) => s,
            Err(err) => {
                log::debug!("invalid utf at {total_read}: {err}");
                invalid_utf += 1;
                line_buf.clear();
                continue;
            }
        };

        total_read += line.as_bytes().len();

        const MAX_PAIRS: usize = 256;
        let mut pairs: [(&str, &str); MAX_PAIRS] = [("", ""); MAX_PAIRS];
        let (n_pairs, overflow) = logfmt::parse_line(line, &mut pairs);
        total_pairs += n_pairs;
        total_overflow += overflow as usize;

        line_buf.clear();
    }

    let stop = Instant::now();

    log::info!(
        "read {total_read} bytes, {total_pairs} pairs ({total_overflow} overflow), \
         {invalid_utf} invalid utf8 lines, {}/s",
        humanize_bytes_binary!(f64::floor(
            total_read as f64 / stop.duration_since(start).as_secs_f64()
        ) as u64)
    );
    Ok(())
}
