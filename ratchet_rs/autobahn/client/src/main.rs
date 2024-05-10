use std::env::current_dir;
use std::fs::File;
use std::io::BufReader;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::{sleep, Instant};

use ratchet_rs::{subscribe, WebSocketConfig};

const PWD_ERR: &str = "Failed to get PWD";
const INVALID_RESULTS: &str = "Invalid results structure";

fn cargo_command() -> Result<Command> {
    let mut pwd = current_dir().context(PWD_ERR)?;
    pwd.push("ratchet_rs");

    let mut cmd = Command::new("cargo");
    cmd.args(&[
        "run",
        "--release",
        "--example",
        "autobahn-client",
        "--features",
        "deflate",
    ])
    .current_dir(pwd);

    Ok(cmd)
}

async fn kill_container() {
    Command::new("docker")
        .args(&["kill", "fuzzingserver"])
        .stdin(Stdio::null())
        .spawn()
        .expect("Failed to kill any lingering container")
        .wait()
        .await
        .expect("Failed to kill any lingering container");
}

fn docker_command() -> Result<Command> {
    let mut pwd = current_dir().context(PWD_ERR)?;
    pwd.push("ratchet_rs");

    // mount /pwd/autobahn/client to /autobahn in the volume
    let mut volume_arg = pwd.clone();
    volume_arg.push("autobahn/client:/autobahn");

    let mut cmd = Command::new("docker");
    cmd.args(&["run", "-d", "--rm", "-v"])
        .arg(volume_arg)
        .args(&[
            "-p",
            "9001:9001",
            "--init",
            "--platform",
            "linux/amd64",
            "--name",
            "fuzzingserver",
            "crossbario/autobahn-testsuite",
            "wstest",
            "-m",
            "fuzzingserver",
            "-s",
        ])
        // spec is now available at this directory due to how the host directory was mounted
        .arg("autobahn/fuzzingserver.json")
        .current_dir(pwd);

    Ok(cmd)
}

#[tokio::main]
async fn main() -> Result<()> {
    let _server_process = tokio::spawn(async move {
        // Just in case there is one lingering after being run locally.
        kill_container().await;

        docker_command()
            .expect(PWD_ERR)
            .spawn()
            .expect("Failed to spawn autobahn server")
            .wait()
            .await
    });

    await_server_start().await?;

    cargo_command()?
        .spawn()
        .context("Failed to spawn docker container")?
        .wait()
        .await
        .context("Failed to run autobahn client")?;

    validate_results().await;
    kill_container().await;

    Ok(())
}

async fn validate_results() -> Result<()> {
    let mut results_file = current_dir().context(PWD_ERR)?;
    results_file.push("ratchet_rs/autobahn/client/results/index.json");

    let file = File::open(results_file).context("Failed to open client results file")?;
    let reader = BufReader::new(file);
    let results = serde_json::from_reader::<_, Value>(reader)?;

    match results {
        Value::Object(map) => {
            let ratchet_results = map["Ratchet"]
                .as_object()
                .expect("Invalid result structure");

            for (_, test) in ratchet_results {
                match test {
                    Value::Object(object) => match object["behavior"].as_str() {
                        Some(result) if result == "OK" => {}
                        _ => bail!(INVALID_RESULTS),
                    },
                    _ => bail!(INVALID_RESULTS),
                }
            }
        }
        _ => bail!(INVALID_RESULTS),
    }

    Ok(())
}

async fn await_server_start() -> Result<()> {
    const TIMEOUT: u64 = 30;
    let start = Instant::now();

    loop {
        match await_handshake().await {
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

async fn await_handshake() -> std::result::Result<(), ratchet_rs::Error> {
    let stream = TcpStream::connect("127.0.0.1:9001").await?;
    subscribe(
        WebSocketConfig::default(),
        stream,
        "ws://localhost:9001/getCaseCount",
    )
    .await
    .map(|_| ())
}
