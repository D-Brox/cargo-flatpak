use clap::Parser;

/// Simple program to greet a person
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Args {
    #[clap(short, long, default_value = "cargo-sources.json")]
    pub output: String,
}

#[derive(Debug, Parser)]
#[clap(bin_name = "cargo")]
pub enum Command {
    Flatpak(Args),
}