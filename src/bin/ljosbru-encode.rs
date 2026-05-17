use clap::Parser;
use env_logger::Env;
use ljosbru::{EncodeArgs, encode};

fn main() -> anyhow::Result<()> {
    init_logger();

    let cli = Cli::parse();
    encode(cli.args)
}

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(flatten)]
    args: EncodeArgs,
}

fn init_logger() {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn"))
        .format_target(false)
        .format_timestamp(None)
        .init();
}
