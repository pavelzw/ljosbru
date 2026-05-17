use std::ffi::OsStr;

use clap::Parser;
use env_logger::Env;
use ljosbru::{DecodeArgs, PrintMissingArgs, decode, print_missing};

const PRINT_MISSING: &str = "print-missing";

fn main() -> anyhow::Result<()> {
    init_logger();

    match CliCommand::parse() {
        CliCommand::Decode(args) => decode(args),
        CliCommand::PrintMissing(args) => print_missing(args),
    }
}

enum CliCommand {
    Decode(DecodeArgs),
    PrintMissing(PrintMissingArgs),
}

impl CliCommand {
    fn parse() -> Self {
        let mut args = std::env::args_os().collect::<Vec<_>>();
        if args
            .get(1)
            .is_some_and(|arg| arg == OsStr::new(PRINT_MISSING))
        {
            args.remove(1);
            return Self::PrintMissing(PrintMissingCli::parse_from(args).args);
        }

        Self::Decode(DecodeCli::parse_from(args).args)
    }
}

#[derive(Debug, Parser)]
#[command(
    version,
    about,
    after_help = "Subcommands:\n  print-missing  Print missing frame IDs from a decode cache"
)]
struct DecodeCli {
    #[command(flatten)]
    args: DecodeArgs,
}

#[derive(Debug, Parser)]
#[command(version, about = "Print missing frame IDs from a decode cache")]
struct PrintMissingCli {
    #[command(flatten)]
    args: PrintMissingArgs,
}

fn init_logger() {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn"))
        .format_target(false)
        .format_timestamp(None)
        .init();
}
