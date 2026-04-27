use humanize_bytes::humanize_bytes_binary;
use smartstring::alias::String as SmartString;
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

    // aggregate all log facilities
    let mut facil_buf = String::new();
    let mut all_facil: vecmap::VecMap<SmartString, usize> = vecmap::VecMap::new();

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

        // The fast parser requires no trailing newline.
        while matches!(line_buf.last(), Some(b'\n' | b'\r')) {
            line_buf.pop();
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

        total_read += n;

        // parse logfmt line
        let mut buf = logfmt::PairsBuffer::<256>::new();
        let (pairs, overflow) = buf.parse(line);
        total_pairs += pairs.len();
        total_overflow += overflow as usize;

        for (k, v) in pairs {
            // list all facilities
            if *k == "facil" {
                facil_buf.clear();
                logfmt::unescape_value(v.as_bytes(), &mut facil_buf);

                let mut key = SmartString::new_const();
                key.push_str(&facil_buf);
                *all_facil.entry(key).or_insert_with(|| 0) += 1;
            }
        }

        line_buf.clear();
    }

    let stop = Instant::now();

    log::info!(
        "read {}, {total_pairs} pairs ({total_overflow} overflow), \
         {invalid_utf} invalid utf8 lines, {}/s",
        humanize_bytes_binary!(total_read),
        humanize_bytes_binary!(f64::floor(
            total_read as f64 / stop.duration_since(start).as_secs_f64()
        ) as u64)
    );

    log::info!("all facil: {all_facil:?}");

    Ok(())
}
