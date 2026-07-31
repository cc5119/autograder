use autograder::cli::{self, Cli};
use clap::{CommandFactory, Parser};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    if cli.version {
        println!("{}", cli::version_string(cli.verbose));
        return;
    }

    let Some(command) = cli.command else {
        Cli::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "a subcommand is required (or pass --version)",
            )
            .exit();
    };

    if let Err(err) = autograder::dispatch(command, cli.verbose) {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}
