use std::env::current_dir;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;
use utils::{await_server_start, cargo_command, validate_results};

const PWD_ERR: &str = "Failed to get PWD";

fn docker_command() -> Result<Command> {
    let mut pwd = current_dir().context(PWD_ERR)?;
    pwd.push("ratchet_rs");

    // mount /pwd/autobahn/server to /autobahn in the volume
    let mut volume_arg = pwd.clone();
    volume_arg.push("autobahn/server:/autobahn");

    let mut cmd = Command::new("docker");
    cmd.args(&["run", "--rm", "-v"])
        .arg(volume_arg)
        .args(&[
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
        .current_dir(pwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    Ok(cmd)
}

#[tokio::main]
async fn main() -> Result<()> {
    let _server_process = tokio::spawn(async move {
        cargo_command("autobahn-server")
            .expect(PWD_ERR)
            .spawn()
            .expect("Failed to spawn autobahn server")
            .wait()
            .await
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
    results_dir.push("ratchet_rs/autobahn/server/results");

    validate_results(results_dir)?;

    Ok(())
}
