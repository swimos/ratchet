use std::env::current_dir;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use utils::{await_server_start, cargo_command, pipe_logs, validate_results};

const PWD_ERR: &str = "Failed to get PWD";
const CONTAINER_NAME: &str = "fuzzingsplitserver";

async fn kill_container() {
    Command::new("docker")
        .args(["kill", CONTAINER_NAME])
        .stdin(Stdio::null())
        .spawn()
        .expect("Failed to spawn command to kill any lingering test container")
        .wait()
        .await
        .expect("Failed to kill any lingering test container");
}

fn docker_command() -> Result<Command> {
    let mut pwd = current_dir().context(PWD_ERR)?;
    pwd.push("ratchet_rs");

    // mount /pwd/autobahn/client to /autobahn in the volume
    let mut volume_arg = pwd.clone();
    volume_arg.push("autobahn/split_client:/autobahn");

    let mut cmd = Command::new("docker");
    cmd.args(["run", "-d", "--rm", "-v"])
        .arg(volume_arg)
        .args([
            "-p",
            "9003:9003",
            "--init",
            "--platform",
            "linux/amd64",
            "--name",
            CONTAINER_NAME,
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

    if let Err(e) = await_server_start(9003).await {
        kill_container().await;
        return Err(e);
    }

    let mut logs_process = match pipe_logs(CONTAINER_NAME).await {
        Ok(child) => child,
        Err(e) => {
            kill_container().await;
            return Err(e);
        }
    };

    cargo_command("autobahn-split-client")?
        .spawn()
        .context("Failed to spawn docker container")?
        .wait()
        .await
        .context("Failed to run autobahn client")?;

    let mut results_dir = current_dir().context(PWD_ERR)?;
    results_dir.push("ratchet_rs/autobahn/split_client/results");

    validate_results(results_dir)?;
    logs_process.kill().await?;
    kill_container().await;

    Ok(())
}
