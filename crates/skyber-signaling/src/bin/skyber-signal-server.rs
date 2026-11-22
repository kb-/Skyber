use std::{env, net::SocketAddr, path::PathBuf, process::ExitCode};

use anyhow::{Context, Result, anyhow};
use skyber_signaling::server;
use tokio::net::TcpListener;

const DEFAULT_LISTEN: &str = "127.0.0.1:34781";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Skyber signaling server failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let options = parse_options(env::args().skip(1).collect())?;
    let listen = value(&options, "--listen")
        .unwrap_or(DEFAULT_LISTEN)
        .parse::<SocketAddr>()
        .map_err(|_| anyhow!("invalid listen address"))?;
    if !listen.ip().is_loopback() && !options.iter().any(|option| option == "--internet") {
        return Err(anyhow!(
            "non-loopback binds require explicit --internet mode"
        ));
    }
    let certificate = std::fs::read(required_path(&options, "--cert")?)
        .context("could not read TLS certificate")?;
    let private_key = std::fs::read(required_path(&options, "--key")?)
        .context("could not read TLS private key")?;
    let tls =
        server::tls_acceptor(&certificate, &private_key).context("could not initialize TLS")?;
    let listener = TcpListener::bind(listen)
        .await
        .context("could not bind signaling listener")?;
    println!("Skyber signaling server listening on {listen} over TLS/WebSocket.");
    server::serve(listener, tls)
        .await
        .context("signaling listener stopped")
}

fn parse_options(args: Vec<String>) -> Result<Vec<String>> {
    let mut result = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let name = &args[index];
        if name == "--internet" {
            result.push(name.clone());
            index += 1;
            continue;
        }
        if !matches!(name.as_str(), "--listen" | "--cert" | "--key") {
            return Err(anyhow!("unknown signaling server option"));
        }
        let option_value = args
            .get(index + 1)
            .context("signaling server option is missing a value")?;
        result.push(name.clone());
        result.push(option_value.clone());
        index += 2;
    }
    Ok(result)
}

fn value<'a>(options: &'a [String], name: &str) -> Option<&'a str> {
    options
        .windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].as_str())
}

fn required_path(options: &[String], name: &str) -> Result<PathBuf> {
    value(options, name)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("required signaling server option is missing"))
}
