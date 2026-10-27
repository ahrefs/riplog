#![deny(unsafe_code)]

use clap::Parser;

fn main() -> anyhow::Result<()> {
    env_logger::try_init()?;

    let cli = riplog::Cli::parse();
    log::debug!("cli: {cli:?}");

    riplog::run(&cli)
}
