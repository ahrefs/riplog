mod cli;
mod logfmt;
mod validate;

use clap::Parser;

use crate::cli::Cli;

fn main() -> anyhow::Result<()> {
    env_logger::try_init()?;

    let cli = cli::Cli::try_parse()?;
    log::debug!("cli: {cli:?}");

    match cli {
        Cli::Validate(v) => match validate::validate(&v) {
            Ok(()) => println!("valid"),
            Err(err) => {
                anyhow::bail!("not valid: {err}")
            }
        },
    }

    Ok(())
}
