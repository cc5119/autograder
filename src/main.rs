use autograder::{cli::Cli, config::Config};
use clap::Parser;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let config = Config::default();
    autograder::dispatch(cli.command, &config)?;
    Ok(())
}
