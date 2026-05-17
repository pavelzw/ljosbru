use clap::{Parser, Subcommand};
use decode::{DecodeArgs, PrintMissingArgs, decode, print_missing};
use encode::{EncodeArgs, encode};
use env_logger::Env;

mod decode;
mod encode;
mod frame;
mod progress;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn"))
        .format_target(false)
        .format_timestamp(None)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Encode(args) => encode(args),
        Command::Decode(args) => decode(args),
        Command::PrintMissing(args) => print_missing(args),
    }
}

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Encode(EncodeArgs),
    Decode(DecodeArgs),
    PrintMissing(PrintMissingArgs),
}
