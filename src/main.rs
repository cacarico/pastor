use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pastor", version, about = "run coding agents on machines you own")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon
    Serve,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let result: anyhow::Result<()> = match cli.command {
        Command::Serve => Err(anyhow::anyhow!("not implemented")),
    };
    if let Err(err) = result {
        fail("runtime_error", &format!("{err:#}"));
    }
}

/// Print a JSON error to stderr and exit 1, matching herdr's CLI convention.
fn fail(code: &str, message: &str) -> ! {
    eprintln!("{}", serde_json::json!({"code": code, "message": message}));
    std::process::exit(1)
}
