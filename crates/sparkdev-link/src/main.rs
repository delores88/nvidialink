mod client;
mod daemon;
mod storage;
mod wire;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{Read, Write};

fn input() -> Result<Value> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(wire::RPC_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= wire::RPC_LIMIT, "request too large");
    Ok(serde_json::from_slice(&bytes).context("invalid JSON input")?)
}

pub fn output(value: Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &value)?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}

#[tokio::main]
async fn main() {
    let result = run().await;
    if let Err(e) = result {
        // The error chain contains only fixed diagnostics; never request/ticket/key bytes.
        let _ = output(json!({"ok":false,"error":{"code":"link_error","message":e.to_string()}}));
        std::process::exit(70);
    }
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str).unwrap_or("status") {
        "serve" => daemon::serve().await,
        "owner-ticket" => daemon::owner_ticket(&args[2..]).await,
        "owner-cancel" => daemon::owner_cancel(&args[2..]).await,
        "identity" => output(
            json!({"ok":true,"device_id":storage::identity(&storage::client_dir()?)?.public().to_string()}),
        ),
        "pair" | "pair-start" => client::pair(input()?).await,
        "rpc" => output(client::rpc(input()?).await?),
        "status" | "pair-status" => output(client::status().await?),
        _ => anyhow::bail!("usage: sparkdev-link serve|owner-ticket|identity|pair|rpc|status"),
    }
}
