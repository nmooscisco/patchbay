//! Container-as-device integration: a containerized service is reachable from a
//! lab device at the container's lab IP.
//!
//! Gated on `podman` being available and pulls a small public image, so it
//! skips cleanly where podman is absent (for example in a minimal CI image).

use std::io::{Read, Write};

use super::*;

/// Returns `true` if a working `podman` is on `PATH`.
fn podman_available() -> bool {
    std::process::Command::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A container attached to a router is reachable from another device on it.
///
/// Runs `nginx:alpine`, waits for port 80 to accept, then fetches `/` from a
/// separate client device and checks for an nginx response.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_service_reachable_from_device() -> Result<()> {
    check_caps()?;
    if !podman_available() {
        eprintln!("skipping container_service_reachable_from_device: podman not on PATH");
        return Ok(());
    }

    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let web = lab
        .add_container("web", "docker.io/library/nginx:alpine")
        .uplink(net.id())
        .ready_tcp(80)
        .build()
        .await?;
    let client = lab.add_device("client").uplink(net.id()).build().await?;

    let web_ip = web.ip().context("container has no ip")?;
    let addr = SocketAddr::new(IpAddr::V4(web_ip), 80);

    // Fetch `/` from inside the client namespace, over the lab network.
    let response = client.run_sync(move || {
        let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .context("connect to container")?;
        stream.write_all(b"GET / HTTP/1.0\r\nHost: web\r\n\r\n")?;
        let mut buf = String::new();
        stream.read_to_string(&mut buf)?;
        Ok(buf)
    })?;

    let status = response.lines().next().unwrap_or_default();
    assert!(
        status.starts_with("HTTP/1.1 200"),
        "expected a 200 from nginx, got status line: {status:?}"
    );

    // Inside the container, root owns the image files, as with plain podman.
    let out = web
        .exec(["stat", "-c", "%u", "/etc/nginx/nginx.conf"])
        .await?;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "0",
        "nginx.conf not owned by root: {out:?}"
    );
    Ok(())
}

/// A read-only bind mount is visible inside the container.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_bind_mount_visible() -> Result<()> {
    check_caps()?;
    if !podman_available() {
        eprintln!("skipping container_bind_mount_visible: podman not on PATH");
        return Ok(());
    }

    let dir = testdir::testdir!();
    std::fs::write(dir.join("hello.txt"), "patchbay-volume").context("write mount file")?;

    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let box_ = lab
        .add_container("box", "docker.io/library/alpine:latest")
        .uplink(net.id())
        .volume_ro(&dir, "/data")
        .args(["sleep", "infinity"])
        .build()
        .await?;

    let out = box_.exec(["cat", "/data/hello.txt"]).await?;
    assert!(
        out.status.success(),
        "cat failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "patchbay-volume"
    );
    Ok(())
}

/// write_file/read_file round-trip and copy_to/copy_from work against a
/// running container.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_file_transfer() -> Result<()> {
    check_caps()?;
    if !podman_available() {
        eprintln!("skipping container_file_transfer: podman not on PATH");
        return Ok(());
    }

    let dir = testdir::testdir!();

    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let box_ = lab
        .add_container("box", "docker.io/library/alpine:latest")
        .uplink(net.id())
        .args(["sleep", "infinity"])
        .build()
        .await?;

    // write_file then read_file round-trips.
    box_.write_file("/tmp/greeting", "hello-from-host").await?;
    assert_eq!(box_.read_file("/tmp/greeting").await?, b"hello-from-host");

    // copy_from a container file to the host.
    box_.copy_from("/tmp/greeting", dir.join("out.txt")).await?;
    assert_eq!(std::fs::read(dir.join("out.txt"))?, b"hello-from-host");

    // copy_to a host file into the container.
    std::fs::write(dir.join("in.txt"), "from-copy-to")?;
    box_.copy_to(dir.join("in.txt"), "/tmp/in.txt").await?;
    assert_eq!(box_.read_file("/tmp/in.txt").await?, b"from-copy-to");
    Ok(())
}

/// Returns whether podman knows a container of this name.
async fn container_exists(name: &str) -> Result<bool> {
    let filter = format!("name=^{name}$");
    let args = ["ps", "-a", "--filter", &filter, "--format", "{{.Names}}"];
    let output = crate::container::podman(args.map(String::from).to_vec()).await?;
    anyhow::ensure!(output.status.success(), "podman ps failed: {output:?}");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == name))
}

/// Waits up to five seconds for a container to be gone.
async fn wait_gone(name: &str) -> Result<()> {
    for _ in 0..50 {
        if !container_exists(name).await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("container {name} still exists")
}

/// `exec` runs in the device's network namespace: it sees the lab address
/// and reaches another lab device by ping.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_exec_uses_the_lab_network() -> Result<()> {
    check_caps()?;
    if !podman_available() {
        eprintln!("skipping container_exec_uses_the_lab_network: podman not on PATH");
        return Ok(());
    }
    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let box_ = lab
        .add_container("box", "docker.io/library/alpine:latest")
        .uplink(net.id())
        .args(["sleep", "infinity"])
        .build()
        .await?;
    let client = lab.add_device("client").uplink(net.id()).build().await?;

    let box_ip = box_.ip().context("container has no ip")?.to_string();
    let out = box_.exec(["ip", "-o", "-4", "addr"]).await?;
    let addrs = String::from_utf8_lossy(&out.stdout);
    assert!(
        addrs.contains(&box_ip),
        "exec does not see {box_ip}: {addrs}"
    );

    let client_ip = client.ip().context("client has no ip")?.to_string();
    let out = box_
        .exec(["ping", "-c", "1", "-W", "2", &client_ip])
        .await?;
    assert!(
        out.status.success(),
        "ping from the container failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// Dropping the last handle removes the container.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_removed_on_drop() -> Result<()> {
    check_caps()?;
    if !podman_available() {
        eprintln!("skipping container_removed_on_drop: podman not on PATH");
        return Ok(());
    }
    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let box_ = lab
        .add_container("box", "docker.io/library/alpine:latest")
        .uplink(net.id())
        .args(["sleep", "infinity"])
        .build()
        .await?;
    let name = box_.container_name().to_string();
    assert!(container_exists(&name).await?);
    drop(box_);
    wait_gone(&name).await
}

/// A failed build removes the container and its device, so the name can be
/// used again.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_failed_build_cleans_up() -> Result<()> {
    check_caps()?;
    if !podman_available() {
        eprintln!("skipping container_failed_build_cleans_up: podman not on PATH");
        return Ok(());
    }
    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let result = lab
        .add_container("box", "docker.io/library/alpine:latest")
        .uplink(net.id())
        .run_arg("--no-such-flag")
        .build()
        .await;
    assert!(result.is_err(), "build with a bad flag should fail");
    assert!(
        lab.device_by_name("box").is_none(),
        "device of failed build remains"
    );

    let box_ = lab
        .add_container("box", "docker.io/library/alpine:latest")
        .uplink(net.id())
        .args(["sleep", "infinity"])
        .build()
        .await?;
    assert!(container_exists(box_.container_name()).await?);
    Ok(())
}

/// A container that exits fails readiness right away, with its logs.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_exit_fails_readiness_early() -> Result<()> {
    check_caps()?;
    if !podman_available() {
        eprintln!("skipping container_exit_fails_readiness_early: podman not on PATH");
        return Ok(());
    }
    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let start = Instant::now();
    let err = lab
        .add_container("box", "docker.io/library/alpine:latest")
        .uplink(net.id())
        .args(["sh", "-c", "echo bye; exit 3"])
        .ready_tcp_timeout(80, Duration::from_secs(30))
        .build()
        .await
        .expect_err("readiness should fail");
    let err = format!("{err:#}");
    assert!(err.contains("exited"), "unexpected error: {err}");
    assert!(err.contains("bye"), "error lacks the logs: {err}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "took {:?}",
        start.elapsed()
    );
    Ok(())
}

/// A container can be built from inside another device's task, whose worker
/// threads carry a stacked `/proc/net` bind mount.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_build_from_device_task() -> Result<()> {
    check_caps()?;
    if !podman_available() {
        eprintln!("skipping container_build_from_device_task: podman not on PATH");
        return Ok(());
    }
    // A device task cannot pull, so make sure the image is present.
    let pull = ["pull", "docker.io/library/alpine:latest"].map(String::from);
    let output = crate::container::podman(pull.to_vec()).await?;
    anyhow::ensure!(output.status.success(), "pull failed: {output:?}");
    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let client = lab.add_device("client").uplink(net.id()).build().await?;
    let net_id = net.id();
    let inner_lab = lab.clone();
    let name = client
        .spawn(move |_| async move {
            let box_ = inner_lab
                .add_container("box", "docker.io/library/alpine:latest")
                .uplink(net_id)
                .args(["sleep", "infinity"])
                .build()
                .await?;
            anyhow::Ok(box_.container_name().to_string())
        })?
        .await??;
    wait_gone(&name).await
}

/// Building from a device task fails clearly when the image would need a pull.
#[tokio::test(flavor = "current_thread")]
#[traced_test]
async fn container_build_from_device_task_needs_a_present_image() -> Result<()> {
    check_caps()?;
    if !podman_available() {
        eprintln!("skipping: podman not on PATH");
        return Ok(());
    }
    let lab = Lab::new().await?;
    let net = lab.add_router("net").build().await?;
    let client = lab.add_device("client").uplink(net.id()).build().await?;
    let net_id = net.id();
    let inner_lab = lab.clone();
    let err = client
        .spawn(move |_| async move {
            inner_lab
                .add_container("box", "localhost/patchbay-test/no-such-image:1")
                .uplink(net_id)
                .build()
                .await
                .map(drop)
        })?
        .await?
        .expect_err("build should fail");
    let err = format!("{err:#}");
    assert!(
        err.contains("cannot be pulled from inside a lab"),
        "unexpected: {err}"
    );
    Ok(())
}
