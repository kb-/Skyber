use std::{env, net::SocketAddr, path::PathBuf, process::ExitCode};

use anyhow::{Context, Result, anyhow, ensure};
use skyber_media_kyber::connectivity;

const DEFAULT_ADDRESS: &str = "127.0.0.1:34780";
const DEFAULT_MESSAGE: &str = "synthetic-connectivity-check";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Gate C-0 failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        return Err(usage());
    };
    match command.as_str() {
        "token-create" => {
            let Some(path) = args.next() else {
                return Err(anyhow!("token-create requires one output path"));
            };
            ensure!(
                args.next().is_none(),
                "token-create requires one output path"
            );
            connectivity::create_token(PathBuf::from(path).as_path())?;
            println!("Gate C-0 token created.");
        }
        "server" => run_server(parse_options(args.collect())?).await?,
        "client" => run_client(parse_options(args.collect())?).await?,
        _ => return Err(usage()),
    }
    Ok(())
}

async fn run_server(options: Vec<String>) -> Result<()> {
    let address = socket_address(&options, "--listen", DEFAULT_ADDRESS)?;
    let internet = options.iter().any(|item| item == "--internet");
    connectivity::validate_address_mode(address, internet)?;
    let certificate = required_path(&options, "--cert")?;
    let private_key = required_path(&options, "--key")?;
    let token_path = required_path(&options, "--token-file")?;
    let token = connectivity::read_token(&token_path)?;

    let listener =
        connectivity::start_listener(address, internet, &certificate, &private_key).await?;
    println!("Gate C-0 listener ready for one probe.");
    connectivity::accept_probe_once(&listener, token.expose()).await?;
    println!("Gate C-0 authenticated exchange completed.");
    Ok(())
}

async fn run_client(options: Vec<String>) -> Result<()> {
    let address = socket_address(&options, "--connect", DEFAULT_ADDRESS)?;
    let internet = options.iter().any(|item| item == "--internet");
    connectivity::validate_address_mode(address, internet)?;
    let pin = required_value(&options, "--pin")?;
    let token_path = required_path(&options, "--token-file")?;
    let token = connectivity::read_token(&token_path)?;
    let payload = value(&options, "--message").unwrap_or(DEFAULT_MESSAGE);
    ensure!(
        payload.len() <= 48,
        "probe message must be at most 48 bytes"
    );

    connectivity::connect_and_probe(address, internet, &pin, token.expose(), payload.as_bytes())
        .await?;
    println!("Gate C-0 authenticated exchange completed.");
    Ok(())
}

fn parse_options(args: Vec<String>) -> Result<Vec<String>> {
    let mut normalized = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let name = &args[index];
        if name == "--internet" {
            normalized.push(name.clone());
            index += 1;
            continue;
        }
        if !matches!(
            name.as_str(),
            "--listen" | "--connect" | "--cert" | "--key" | "--token-file" | "--pin" | "--message"
        ) {
            return Err(anyhow!("unknown Gate C-0 option"));
        }
        let option_value = args
            .get(index + 1)
            .context("Gate C-0 option is missing a value")?;
        normalized.push(name.clone());
        normalized.push(option_value.clone());
        index += 2;
    }
    Ok(normalized)
}

fn value<'a>(options: &'a [String], name: &str) -> Option<&'a str> {
    options
        .windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].as_str())
}

fn required_value(options: &[String], name: &str) -> Result<String> {
    value(options, name)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("required Gate C-0 option is missing"))
}

fn required_path(options: &[String], name: &str) -> Result<PathBuf> {
    required_value(options, name).map(PathBuf::from)
}

fn socket_address(options: &[String], name: &str, default: &str) -> Result<SocketAddr> {
    let text = value(options, name).unwrap_or(default);
    text.parse()
        .map_err(|_| anyhow!("invalid socket address; use host:port or bracketed IPv6 [host]:port"))
}

fn usage() -> anyhow::Error {
    anyhow!(
        "usage: skyber-gate-c0 token-create <path> | server [--listen ADDR] --cert PEM --key PEM --token-file PATH [--internet] | client [--connect ADDR] --pin SHA256 --token-file PATH [--message TEXT] [--internet]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_reject_unknown_and_allow_explicit_ipv6_address() -> Result<()> {
        assert!(parse_options(vec!["--unexpected".into()]).is_err());
        let options = parse_options(vec!["--connect".into(), "[::1]:34780".into()])?;
        assert_eq!(
            socket_address(&options, "--connect", DEFAULT_ADDRESS)?,
            "[::1]:34780".parse()?
        );
        Ok(())
    }

    #[test]
    fn options_require_values() {
        assert!(parse_options(vec!["--token-file".into()]).is_err());
    }
}
