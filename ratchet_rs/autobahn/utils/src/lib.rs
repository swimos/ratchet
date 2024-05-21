use std::env::current_dir;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use anyhow::{bail, Context};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::Instant;

use ratchet_rs::{subscribe, WebSocketConfig};

const PWD_ERR: &str = "Failed to get PWD";

pub fn cargo_command(example: &str) -> Result<Command> {
    let mut pwd = current_dir().context(PWD_ERR)?;
    pwd.push("ratchet_rs");

    let mut cmd = Command::new("cargo");
    cmd.args(["run", "--release", "--example", example, "--all-features"])
        .current_dir(pwd)
        .env("RUST_BACKTRACE", "FULL");

    Ok(cmd)
}

pub fn validate_results(mut dir: PathBuf) -> Result<()> {
    dir.push("index.json");

    let file = File::open(dir).context("Failed to open client results file")?;
    let reader = BufReader::new(file);
    let results = serde_json::from_reader::<_, Value>(reader)?;

    match results {
        Value::Object(map) => {
            let ratchet_results = map["Ratchet"]
                .as_object()
                .expect("Invalid result structure");

            let mut failures = Vec::new();

            for (test_id, test) in ratchet_results {
                match test {
                    Value::Object(object) => match object["behavior"].as_str() {
                        Some(result) => {
                            if !["OK", "INFORMATIONAL", "NON-STRICT"].contains(&result) {
                                failures.push(format!("Test {test_id} failed with: {result}"));
                            }
                        }
                        _ => bail!("Invalid results structure"),
                    },
                    _ => bail!("Invalid results structure"),
                }
            }

            if !failures.is_empty() {
                eprintln!("Autobahn test suite encountered failures:");

                for test in failures {
                    eprintln!("\t{test}");
                }

                bail!("Autobahn test suite failure")
            } else {
                println!("All tests passed");
            }
        }
        _ => bail!("Invalid results structure"),
    }

    Ok(())
}

pub async fn await_server_start(port: u64) -> Result<()> {
    const TIMEOUT: u64 = 30;
    let start = Instant::now();

    loop {
        match await_handshake(port).await {
            Ok(()) => break Ok(()),
            Err(_) => {
                if start.elapsed() > Duration::from_secs(TIMEOUT) {
                    bail!("Autobahn Server failed to start within {TIMEOUT} seconds");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn await_handshake(port: u64) -> Result<(), ratchet_rs::Error> {
    let stream = TcpStream::connect(format!("127.0.0.1:{port}")).await?;
    subscribe(
        WebSocketConfig::default(),
        stream,
        format!("ws://localhost:{port}/getCaseCount"),
    )
    .await
    .map(|_| ())
}
