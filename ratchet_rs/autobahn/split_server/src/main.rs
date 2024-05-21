use std::env::current_dir;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::process::Command;
use tokio::sync::Notify;
use utils::{await_server_start, cargo_command, validate_results};

const PWD_ERR: &str = "Failed to get PWD";

fn docker_command() -> Result<Command> {
    let mut pwd = current_dir().context(PWD_ERR)?;
    pwd.push("ratchet_rs");

    // mount /pwd/autobahn/server to /autobahn in the volume
    let mut volume_arg = pwd.clone();
    volume_arg.push("autobahn/split_server:/autobahn");

    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "-v"])
        .arg(volume_arg)
        .args([
            "--network",
            "host",
            "--platform",
            "linux/amd64",
            "crossbario/autobahn-testsuite:0.8.2",
            "wstest",
            "-m",
            "fuzzingclient",
            "-s",
        ])
        // spec is now available at this directory due to how the host directory was mounted
        .arg("autobahn/fuzzingclient.json")
        .current_dir(pwd);

    Ok(cmd)
}

#[tokio::main]
async fn main() -> Result<()> {
    let stop = Arc::new(Notify::new());
    let server_stop = stop.clone();

    let server_process = tokio::spawn(async move {
        let mut child = cargo_command("autobahn-split-server")
            .expect(PWD_ERR)
            .spawn()
            .expect("Failed to spawn autobahn server");
        server_stop.notified().await;

        child.kill().await.expect("Failed to kill process");
    });

    await_server_start(9002).await?;

    let result = docker_command()?
        .spawn()
        .context("Failed to spawn docker container")?
        .wait()
        .await
        .context("Autobahn suite failed")?;

    if !result.success() {
        bail!("Autobahn suite failed");
    }

    let mut results_dir = current_dir().context(PWD_ERR)?;
    results_dir.push("ratchet_rs/autobahn/split_server/results");

    validate_results(results_dir)?;

    stop.notify_waiters();
    server_process.await.expect("Cargo server process failed");

    Ok(())
}
