//! omniroute-rust entry point (parity: `bin/omniroute.mjs`).

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "omniroute",
    version = omniroute_rust::VERSION,
    about = "OmniRoute AI gateway (Rust rewrite): one endpoint, multi-provider routing with quota-aware fallback"
)]
struct Cli {
    /// output format: table (default) or json
    #[arg(long, global = true, value_parser = ["table", "json"], default_value = "table")]
    output: String,

    #[arg(long, global = true, value_name = "KEY", env = "OMNIROUTE_API_KEY")]
    api_key: Option<String>,

    /// base URL of a running gateway (defaults to http://127.0.0.1:<port>)
    #[arg(long, global = true, value_name = "URL")]
    base_url: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the gateway (default subcommand)
    Serve {
        /// TCP port (defaults: --port > PORT env > 20128)
        #[arg(long)]
        port: Option<u16>,
        /// bind address
        #[arg(long)]
        host: Option<String>,
    },
    /// Show gateway status (pid + health)
    Status {
        #[arg(long, default_value_t = omniroute_rust::config::DEFAULT_PORT)]
        port: u16,
    },
    /// Stop a running gateway (pidfile)
    Stop,
    /// List models from a running gateway
    Models {
        #[arg(long, default_value_t = omniroute_rust::config::DEFAULT_PORT)]
        port: u16,
    },
    /// List providers from a running gateway
    Providers {
        #[arg(long, default_value_t = omniroute_rust::config::DEFAULT_PORT)]
        port: u16,
    },
    /// Show configured combos (from the local config file)
    Combos,
    /// Validate config, data dir, and credentials
    Doctor,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    omniroute_rust::set_json_output(cli.output == "json");

    let result = match &cli.command {
        Some(Command::Serve { port, host }) => omniroute_rust::cli::serve(*port, host.clone()).await,
        Some(Command::Status { port }) => {
            let _ = omniroute_rust::cli::status(*port).await;
            Ok(())
        }
        Some(Command::Stop) => omniroute_rust::cli::stop().await,
        Some(Command::Models { port }) => omniroute_rust::cli::models(cli.base_url.clone(), *port, cli.api_key.clone()).await,
        Some(Command::Providers { port }) => {
            omniroute_rust::cli::providers(cli.base_url.clone(), *port, cli.api_key.clone()).await
        }
        Some(Command::Combos) => omniroute_rust::cli::combos().await,
        Some(Command::Doctor) => omniroute_rust::cli::doctor().await,
        None => {
            // default subcommand = serve
            omniroute_rust::cli::serve(None, None).await
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
