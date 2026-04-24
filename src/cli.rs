use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
pub struct Validate {
    pub file: PathBuf,
}

#[derive(Parser, Debug)]
pub enum Cli {
    Validate(Validate),
}
