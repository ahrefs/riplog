#![deny(unsafe_code)]

mod bisect;
mod cli;
mod filter;
mod json;
mod logfmt;
mod output;
mod parallel;
mod run;
mod sort;
mod timestamp;
mod transform;

use clap::Parser;

fn main() -> anyhow::Result<()> {
    env_logger::try_init()?;

    let cli = cli::Cli::try_parse()?;
    log::debug!("cli: {cli:?}");

    run::run(&cli)
}
