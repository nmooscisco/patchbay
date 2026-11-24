//! Containers of a killed patchbay process are removed by the next one.
//!
//! The test process itself stays outside any user namespace and runs its own
//! binary twice as a patchbay process: once to start a container and abort,
//! once to start another container, which reaps the first. Host podman then
//! checks the result, like a user's shell would.

use std::{process::Command, time::Duration};

use anyhow::{bail, ensure, Context, Result};

/// Role of a child process: `start` or `reap`.
const ROLE_ENV: &str = "PATCHBAY_TEST_REAP_ROLE";

#[ctor::ctor(unsafe)]
fn init() {
    if std::env::var_os(ROLE_ENV).is_some() {
        patchbay::init_userns().expect("user namespace");
    }
}

/// A container whose first process runs as another user, left behind by a
/// killed process, is removed when the next process builds a container.
#[tokio::test(flavor = "current_thread")]
async fn container_reaped_after_owner_killed() -> Result<()> {
    match std::env::var(ROLE_ENV).as_deref() {
        Ok("start") => return start_and_abort().await,
        Ok("reap") => return start_one().await.map(drop),
        _ => {}
    }
    if !host_podman(&["--version"]).is_ok_and(|out| out.status.success()) {
        eprintln!("skipping: podman not on PATH");
        return Ok(());
    }
    if !has_subid_entries() {
        eprintln!("skipping: no /etc/subuid and /etc/subgid entries");
        return Ok(());
    }

    let output = child("start")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let name = stdout
        .lines()
        .find_map(|line| line.strip_prefix("CONTAINER "))
        .with_context(|| {
            format!(
                "start child printed no container name:\n{stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            )
        })?
        .to_string();
    let result = check_reaped(&name);
    if result.is_err() {
        cleanup(&name);
    }
    result
}

/// Checks that the container runs, then runs the reaping child and checks that
/// the container is gone.
fn check_reaped(name: &str) -> Result<()> {
    ensure!(exists(name)?, "container {name} is gone before the reap");
    let output = child("reap")?;
    ensure!(
        output.status.success(),
        "reap child failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for _ in 0..50 {
        if !exists(name)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("container {name} still exists after the reap")
}

/// In the `start` child: starts a container whose first process runs as
/// `nobody`, prints its name and aborts, so no drop runs.
async fn start_and_abort() -> Result<()> {
    let lab = patchbay::Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let container = lab
        .add_container("box", "docker.io/library/alpine:latest")
        .uplink(net.id())
        .run_arg("--user=65534:65534")
        .args(["sleep", "infinity"])
        .build()
        .await?;
    println!("CONTAINER {}", container.container_name());
    std::process::abort();
}

/// In the `reap` child: starts and drops a container. The first build in a
/// process reaps orphans.
async fn start_one() -> Result<patchbay::Container> {
    let lab = patchbay::Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    lab.add_container("box", "docker.io/library/alpine:latest")
        .uplink(net.id())
        .args(["true"])
        .build()
        .await
}

/// Runs this test in a child process with the given role.
fn child(role: &str) -> Result<std::process::Output> {
    Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "container_reaped_after_owner_killed",
            "--nocapture",
        ])
        .env(ROLE_ENV, role)
        .output()
        .context("run child")
}

/// Runs podman as the host user, outside any patchbay namespace.
fn host_podman(args: &[&str]) -> Result<std::process::Output> {
    Command::new("podman")
        .args(args)
        .output()
        .context("run podman")
}

/// Returns whether host podman lists a container of this name.
fn exists(name: &str) -> Result<bool> {
    let filter = format!("name=^{name}$");
    let output = host_podman(&["ps", "-a", "--filter", &filter, "--format", "{{.Names}}"])?;
    ensure!(output.status.success(), "podman ps failed: {output:?}");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == name))
}

/// Removes a left-over container after a failure. Host podman cannot signal
/// it, but the host user can, so kill its first process directly.
fn cleanup(name: &str) {
    if let Ok(output) = host_podman(&["inspect", "--format", "{{.State.Pid}}", name]) {
        let pid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if pid != "0" && !pid.is_empty() {
            let _ = Command::new("kill").args(["-9", &pid]).status();
        }
    }
    let _ = host_podman(&["rm", "-f", "-t", "0", name]);
}

/// Returns whether `/etc/subuid` and `/etc/subgid` have an entry for the
/// current user, by name or uid.
fn has_subid_entries() -> bool {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() }.to_string();
    let user = std::env::var("USER").unwrap_or_default();
    let has_entry = |path: &str| {
        std::fs::read_to_string(path).is_ok_and(|text| {
            text.lines().any(|line| {
                let login = line.split(':').next().unwrap_or_default();
                login == uid || (!user.is_empty() && login == user)
            })
        })
    };
    has_entry("/etc/subuid") && has_entry("/etc/subgid")
}
