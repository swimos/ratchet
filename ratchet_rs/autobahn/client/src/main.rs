use std::env::current_dir;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use utils::{await_server_start, cargo_command, validate_results};

const PWD_ERR: &str = "Failed to get PWD";

async fn kill_container() {
    Command::new("docker")
        .args(["kill", "fuzzingserver"])
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
    cmd.args(["run", "-d", "--rm", "-v"])
        .arg(volume_arg)
        .args([
            "-p",
            "9001:9001",
            "--init",
            "--platform",
            "linux/amd64",
            "--name",
            "fuzzingserver",
            "crossbario/autobahn-testsuite:0.8.2",
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

    await_server_start(9001).await?;

    cargo_command("autobahn-client")?
        .spawn()
        .context("Failed to spawn docker container")?
        .wait()
        .await
        .context("Failed to run autobahn client")?;

    let mut results_dir = current_dir().context(PWD_ERR)?;
    results_dir.push("ratchet_rs/autobahn/client/results");

    validate_results(results_dir)?;
    kill_container().await;

    Ok(())
}
