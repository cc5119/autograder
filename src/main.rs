use autograder::cli::Cli;
use clap::Parser;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    if let Err(err) = autograder::dispatch(cli.command) {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}
