//! Run OCI containers as lab devices.
//!
//! See [`Container`] for the user-facing documentation. This module also holds
//! the user-namespace plumbing that lets podman run inside patchbay's own
//! rootless namespace: see [`PodmanUserns`].

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::Deref,
    os::{
        fd::{AsRawFd, BorrowedFd, OwnedFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::OnceCell;
use tracing::{debug, warn};

use crate::{device::DeviceBuilder, Device};

/// The container runtime binary. Only podman works, see [`Container`].
const PODMAN: &str = "podman";

/// How long [`ContainerBuilder::ready_tcp`] waits for the service to accept a
/// connection before giving up.
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Container label that records the patchbay process that started it.
const OWNER_LABEL: &str = "patchbay.owner";

/// Readiness gate applied after the container starts.
#[derive(Clone, Debug)]
enum Ready {
    /// No readiness check; `build` returns as soon as the container starts.
    None,
    /// Wait until a TCP connection to this port on the container's IP succeeds.
    Tcp { port: u16, timeout: Duration },
}

/// A mount to add to the container.
#[derive(Clone, Debug)]
enum Mount {
    /// Bind-mount a host path at `container` path, read-only when `read_only`.
    Bind {
        host: PathBuf,
        container: String,
        read_only: bool,
    },
    /// Mount a fresh tmpfs at `container` path.
    Tmpfs { container: String },
}

impl Mount {
    /// Renders this mount as `podman run` arguments.
    fn to_args(&self) -> Vec<String> {
        match self {
            Mount::Bind {
                host,
                container,
                read_only,
            } => {
                let mut spec = format!("{}:{container}", host.display());
                if *read_only {
                    spec.push_str(":ro");
                }
                vec!["--volume".to_string(), spec]
            }
            Mount::Tmpfs { container } => vec!["--tmpfs".to_string(), container.clone()],
        }
    }
}

/// Builder for a [`Container`]; returned by [`Lab::add_container`](crate::Lab::add_container).
///
/// The uplink and interface methods mirror [`Lab::add_device`](crate::Lab::add_device):
/// a container is a device, so it is wired into the topology the same way.
#[must_use = "a ContainerBuilder does nothing until you call build"]
pub struct ContainerBuilder {
    device: DeviceBuilder,
    image: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    mounts: Vec<Mount>,
    run_args: Vec<String>,
    ready: Ready,
}

impl ContainerBuilder {
    /// Wraps a device builder with container configuration.
    pub(crate) fn new(device: DeviceBuilder, image: impl Into<String>) -> Self {
        Self {
            device,
            image: image.into(),
            args: Vec::new(),
            env: Vec::new(),
            mounts: Vec::new(),
            run_args: Vec::new(),
            ready: Ready::None,
        }
    }

    /// Adds an auto-named interface (eth0, eth1, ...) with the given config.
    ///
    /// Accepts anything that converts to [`IfaceConfig`](crate::IfaceConfig),
    /// including a bare [`NodeId`](crate::NodeId) for a simple routed uplink.
    pub fn uplink(mut self, config: impl Into<crate::IfaceConfig>) -> Self {
        self.device = self.device.uplink(config);
        self
    }

    /// Adds a named interface with the given configuration.
    pub fn iface(mut self, ifname: &str, config: impl Into<crate::IfaceConfig>) -> Self {
        self.device = self.device.iface(ifname, config);
        self
    }

    /// Overrides which interface carries the default route.
    pub fn default_via(mut self, ifname: &str) -> Self {
        self.device = self.device.default_via(ifname);
        self
    }

    /// Sets the MTU on all interfaces of the container device.
    pub fn mtu(mut self, mtu: u32) -> Self {
        self.device = self.device.mtu(mtu);
        self
    }

    /// Sets an environment variable inside the container.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Appends an argument to the container's command (after the image name).
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments to the container's command.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Bind-mounts a host path into the container, read-write.
    ///
    /// Use this to hand the service its data or config, for example a Pebble
    /// config file or a seeded database directory. The host path should be
    /// absolute. For read-only, use [`volume_ro`](Self::volume_ro).
    pub fn volume(mut self, host: impl Into<PathBuf>, container: impl Into<String>) -> Self {
        self.mounts.push(Mount::Bind {
            host: host.into(),
            container: container.into(),
            read_only: false,
        });
        self
    }

    /// Bind-mounts a host path into the container, read-only.
    pub fn volume_ro(mut self, host: impl Into<PathBuf>, container: impl Into<String>) -> Self {
        self.mounts.push(Mount::Bind {
            host: host.into(),
            container: container.into(),
            read_only: true,
        });
        self
    }

    /// Mounts a fresh tmpfs at the given path inside the container.
    ///
    /// Useful for a writable scratch directory on an otherwise read-only image.
    pub fn tmpfs(mut self, container: impl Into<String>) -> Self {
        self.mounts.push(Mount::Tmpfs {
            container: container.into(),
        });
        self
    }

    /// Passes an extra flag to `podman run` (before the image name).
    ///
    /// An escape hatch for flags the builder does not model, such as
    /// `--cgroup-manager=cgroupfs` or `--security-opt=label=disable` for bind
    /// mounts on an SELinux host.
    pub fn run_arg(mut self, arg: impl Into<String>) -> Self {
        self.run_args.push(arg.into());
        self
    }

    /// Waits, after start, until a TCP connection to `port` succeeds.
    ///
    /// The check connects to the container's own lab IP (IPv4, or IPv6 on a
    /// device without IPv4) from inside the container's network namespace. It
    /// shows that the service listens on that address, not that routes to it
    /// work. Fails early if the container exits. Uses a 30-second timeout. See
    /// [`ready_tcp_timeout`](Self::ready_tcp_timeout) to change it.
    pub fn ready_tcp(mut self, port: u16) -> Self {
        self.ready = Ready::Tcp {
            port,
            timeout: DEFAULT_READY_TIMEOUT,
        };
        self
    }

    /// Like [`ready_tcp`](Self::ready_tcp) with an explicit timeout.
    pub fn ready_tcp_timeout(mut self, port: u16, timeout: Duration) -> Self {
        self.ready = Ready::Tcp { port, timeout };
        self
    }

    /// Wires the container device, starts the container, and waits for
    /// readiness.
    ///
    /// # Errors
    ///
    /// Returns an error if patchbay's user namespace has no subordinate id
    /// range (see [`Container`]), if the device fails to wire, if the runtime
    /// binary is missing or `run` fails (the error includes the runtime's
    /// stderr), or if a [`ready_tcp`](Self::ready_tcp) gate times out.
    pub async fn build(self) -> Result<Container> {
        PodmanUserns::get()?;
        let name = container_name(&self.device)?;
        reap_orphans_once().await;

        // Pull on the host before wiring the (registry-unreachable) device
        // namespace.
        ensure_image(&self.image).await?;
        let lab = self.device.inner.clone();
        let device = self.device.build().await?;
        let id = device.id();
        let result = start(
            &device,
            name,
            &self.image,
            &self.env,
            &self.mounts,
            &self.args,
            &self.run_args,
            &self.ready,
        )
        .await;
        if result.is_err() {
            // The container is already removed. Remove its device too, so the
            // caller can retry under the same name.
            if let Err(err) = lab.remove_device(id) {
                warn!(%err, "could not remove the device of a failed container");
            }
        }
        result
    }
}

/// Starts the container in its built device and waits for readiness.
///
/// On error, the container is removed.
#[allow(clippy::too_many_arguments)]
async fn start(
    device: &Device,
    name: String,
    image: &str,
    env: &[(String, String)],
    mounts: &[Mount],
    args: &[String],
    run_args: &[String],
    ready: &Ready,
) -> Result<Container> {
    // Container root is root of a nested user namespace, without
    // capabilities in the device's network namespace. Let it bind low ports
    // and send pings anyway.
    let (low, high) = ping_group_range(&read_id_map("/proc/self/gid_map")?)?;
    device.run_sync(move || {
        crate::wiring::set_sysctl_root("net/ipv4/ip_unprivileged_port_start", "0")?;
        crate::wiring::set_sysctl_root("net/ipv4/ping_group_range", &format!("{low} {high}"))
    })?;

    let run_args = build_run_args(&name, image, env, mounts, args, run_args);
    debug!(container = %name, %image, "starting container");
    // Spawn from inside the device namespace so the container, run with
    // `--network=host`, joins it.
    let output = podman_in(device, run_args)
        .await
        .with_context(|| format!("start container '{name}'"))?;
    // From here on, dropping `container` removes it, also on error.
    let container = Container {
        device: device.clone(),
        inner: Arc::new(ContainerInner { name }),
    };
    if !output.status.success() {
        bail!(
            "`podman run` for container '{}' failed: {}",
            container.inner.name,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    container.wait_ready(ready).await?;
    Ok(container)
}

/// Builds a host-unique podman container name.
///
/// The name holds the owner tag (pid and start time, unique on the host), the
/// device namespace (unique within the process) and the device name, so
/// `podman ps` shows which lab device a container belongs to.
fn container_name(device: &DeviceBuilder) -> Result<String> {
    let inner = device.inner.core.lock().expect("poisoned");
    let dev = inner
        .device(device.id)
        .with_context(|| format!("device {} is gone", device.id))?;
    let tag = owner_tag().replace(':', "-");
    Ok(sanitize_name(&format!(
        "patchbay-{tag}-{}-{}",
        dev.ns, dev.name
    )))
}

/// Replaces characters podman does not accept in a container name with `-`.
fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Builds the argument vector for `<runtime> run`.
///
/// Detached (`-d`), joined to the caller's network namespace (`--network=host`),
/// and labelled with the owning process so [`reap_orphans`] can remove it if
/// that process dies without cleanup.
fn build_run_args(
    name: &str,
    image: &str,
    env: &[(String, String)],
    mounts: &[Mount],
    cmd_args: &[String],
    extra_run_args: &[String],
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--network=host".to_string(),
        // The image is pre-pulled on the host; the device namespace has no
        // route to a registry.
        "--pull=never".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--label".to_string(),
        format!("{OWNER_LABEL}={}", owner_tag()),
    ];
    for (k, v) in env {
        args.push("--env".to_string());
        args.push(format!("{k}={v}"));
    }
    for mount in mounts {
        args.extend(mount.to_args());
    }
    args.extend(extra_run_args.iter().cloned());
    args.push(image.to_string());
    args.extend(cmd_args.iter().cloned());
    args
}

/// A user namespace nested in patchbay's, laid out like podman's own rootless
/// namespace: inner 0 is the host user and inner 1.. is the user's subordinate
/// id range.
///
/// Every podman process joins it (see [`runtime_command`]) and runs as its root.
/// To podman this is the state after its own rootless re-exec, so it uses the
/// user's normal rootless storage and its images. patchbay's own namespace
/// keeps the user's uid (see [`init_userns`](crate::init_userns)), so the
/// nested map puts the user back on 0 and the subordinate ids, in host order,
/// on 1.. (see [`nested_id_map`]).
///
/// The environment variables in [`runtime_command`] tell podman that it is in
/// its configured namespace. They are internal but stable, and exactly what
/// podman sets on its own re-exec. podman cannot set up this namespace itself:
/// its rootless re-exec calls `newuidmap`, whose setuid bit has no effect inside
/// patchbay's namespace. Making "run in an already-configured user namespace" a
/// supported flag is the open request containers/podman#7774.
///
/// Container root has no capabilities in the device's network namespace, which
/// belongs to patchbay's namespace. That keeps containers from changing the
/// lab's routes or firewall. [`ContainerBuilder::build`] sets per-namespace
/// sysctls so services can still bind low ports and ping.
struct PodmanUserns {
    /// `/proc/<pid>/ns/user` of the namespace.
    fd: OwnedFd,
    /// Host uid that inner 0 maps to.
    host_uid: u32,
    /// Host gid that inner 0 maps to.
    host_gid: u32,
}

impl PodmanUserns {
    /// Returns the namespace, creating it on first use.
    fn get() -> Result<&'static Self> {
        static USERNS: OnceLock<std::result::Result<PodmanUserns, String>> = OnceLock::new();
        USERNS
            .get_or_init(|| Self::create().map_err(|e| format!("{e:#}")))
            .as_ref()
            .map_err(|e| anyhow!("{e}"))
    }

    fn create() -> Result<Self> {
        use rustix::{
            io::{read, write},
            pipe::pipe,
            process::{getegid, geteuid, waitpid, Pid, WaitOptions},
            thread::UnshareFlags,
        };

        let uid_map = read_id_map("/proc/self/uid_map")?;
        let gid_map = read_id_map("/proc/self/gid_map")?;
        let (uid_lines, host_uid) = nested_id_map(&uid_map, geteuid().as_raw())?;
        let (gid_lines, host_gid) = nested_id_map(&gid_map, getegid().as_raw())?;

        // c2p tells the parent that the child unshared; p2c releases the child
        // once the parent has written its maps and opened the namespace.
        let (c2p_r, c2p_w) = pipe().context("pipe")?;
        let (p2c_r, p2c_w) = pipe().context("pipe")?;

        // SAFETY: the child only makes system calls (unshare, write, read,
        // _exit) and does not allocate, so forking a threaded process is sound.
        let child = unsafe { libc::fork() };
        if child < 0 {
            return Err(std::io::Error::last_os_error()).context("fork");
        }
        if child == 0 {
            drop(c2p_r);
            drop(p2c_w);
            let ok = crate::netns::unshare_namespaces(UnshareFlags::NEWUSER).is_ok();
            let _ = write(&c2p_w, &[u8::from(ok)]);
            let _ = read(&p2c_r, &mut [0u8; 1]);
            // SAFETY: exit without running atexit handlers in the forked child.
            unsafe { libc::_exit(0) };
        }

        drop(c2p_w);
        drop(p2c_r);
        let fd = (|| -> Result<OwnedFd> {
            let mut status = [0u8; 1];
            let got = read(&c2p_r, &mut status).context("await child")?;
            if got != 1 || status[0] != 1 {
                bail!("unshare(CLONE_NEWUSER) failed in the helper process");
            }
            std::fs::write(format!("/proc/{child}/uid_map"), uid_lines)
                .context("write nested uid_map")?;
            std::fs::write(format!("/proc/{child}/gid_map"), gid_lines)
                .context("write nested gid_map")?;
            let ns = std::fs::File::open(format!("/proc/{child}/ns/user"))
                .context("open nested user namespace")?;
            Ok(ns.into())
        })();
        drop(p2c_w);
        let _ = waitpid(Pid::from_raw(child), WaitOptions::empty());
        let fd = fd.context("create the user namespace for podman")?;
        debug!(host_uid, host_gid, "created podman user namespace");
        Ok(Self {
            fd,
            host_uid,
            host_gid,
        })
    }
}

/// One `inner outer count` line of a `/proc/<pid>/{uid,gid}_map`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IdMapLine {
    inner: u32,
    outer: u32,
    count: u32,
}

impl IdMapLine {
    fn contains_inner(&self, id: u32) -> bool {
        id >= self.inner && id - self.inner < self.count
    }
}

/// Reads and parses an id map file.
fn read_id_map(path: &str) -> Result<Vec<IdMapLine>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    parse_id_map(&text).with_context(|| format!("parse {path}"))
}

fn parse_id_map(text: &str) -> Result<Vec<IdMapLine>> {
    text.lines()
        .map(|line| {
            let fields = line
                .split_whitespace()
                .map(str::parse)
                .collect::<std::result::Result<Vec<u32>, _>>()?;
            let [inner, outer, count] = fields[..] else {
                bail!("expected three fields: {line:?}");
            };
            Ok(IdMapLine {
                inner,
                outer,
                count,
            })
        })
        .collect()
}

/// Builds the map for [`PodmanUserns`] from patchbay's own map.
///
/// `own` is patchbay's id inside its namespace. The result maps inner 0 to it,
/// and inner 1.. to the other mapped ids in the order of their host ids. With
/// patchbay's layout (user to itself, 0 to the first subordinate id, the rest
/// of the range to itself) inner N is then subordinate id N - 1, as in a plain
/// rootless podman namespace. Returns the map and the host id of `own`.
fn nested_id_map(map: &[IdMapLine], own: u32) -> Result<(String, u32)> {
    let own_line = map
        .iter()
        .find(|line| line.contains_inner(own))
        .with_context(|| format!("id {own} is not mapped"))?;
    let host = own_line.outer + (own - own_line.inner);
    let mut others: Vec<&IdMapLine> = map.iter().filter(|line| *line != own_line).collect();
    if others.is_empty() {
        bail!(
            "patchbay's user namespace has no subordinate id range. Containers need \
             newuidmap/newgidmap on PATH and an /etc/subuid and /etc/subgid entry for \
             the user"
        );
    }
    others.sort_by_key(|line| line.outer);
    let mut lines = format!("0 {own} 1\n");
    let mut next = 1;
    for line in others {
        lines.push_str(&format!("{next} {} {}\n", line.inner, line.count));
        next += line.count;
    }
    Ok((lines, host))
}

/// Returns the `net.ipv4.ping_group_range` that allows every mapped gid.
///
/// The kernel turns both ends into host gids and allows the host gids between
/// them. The range therefore runs from the inner gid with the lowest host gid
/// to the inner gid with the highest.
fn ping_group_range(map: &[IdMapLine]) -> Result<(u32, u32)> {
    let low = map
        .iter()
        .min_by_key(|line| line.outer)
        .context("empty gid map")?;
    let high = map
        .iter()
        .max_by_key(|line| line.outer + (line.count - 1))
        .context("empty gid map")?;
    let high_end = high.inner + (high.count - 1);
    if high_end < low.inner {
        // Inner and host order disagree, so no single range covers both.
        // Allow the lowest line, which holds the user's own gid, the group of
        // container root.
        return Ok((low.inner, low.inner + (low.count - 1)));
    }
    Ok((low.inner, high_end))
}

/// Builds a runtime command that runs in [`PodmanUserns`] as its root, in a
/// mount namespace of its own.
///
/// podman makes its storage mounts private, which needs a mount namespace
/// owned by its user namespace. The command keeps the caller's network
/// namespace, so a command spawned from a device's worker runs in the device.
///
/// Device workers bind-mount `/proc/thread-self/net` over `/proc/net`, which
/// is a symlink, so the mount lands on `/proc/<patchbay pid>/net`. A mount
/// namespace owned by the nested user namespace would lock that mount, and the
/// kernel refuses a new `proc` mount (which the container needs) while a
/// locked mount covers part of `/proc`. So the child first takes a mount
/// namespace of its own in patchbay's user namespace and detaches that mount,
/// as often as it is stacked (a worker created from another worker's thread
/// inherits its mount). The child is single-threaded, so its `/proc/net`
/// shows its own network namespace without the bind mount.
fn runtime_command() -> Result<Command> {
    use rustix::thread::{move_into_link_name_space, LinkNameSpaceType, UnshareFlags};

    let userns = PodmanUserns::get()?;
    let fd = userns.fd.as_raw_fd();
    let proc_net = std::ffi::CString::new(format!("/proc/{}/net", std::process::id()))
        .expect("path has no NUL byte");
    let mut cmd = Command::new(PODMAN);
    cmd.env("_CONTAINERS_USERNS_CONFIGURED", "done")
        .env("_CONTAINERS_ROOTLESS_UID", userns.host_uid.to_string())
        .env("_CONTAINERS_ROOTLESS_GID", userns.host_gid.to_string());
    // SAFETY: the closure only makes system calls and does not allocate. The
    // fd stays open for the life of the process.
    unsafe {
        cmd.pre_exec(move || {
            crate::netns::unshare_namespaces(UnshareFlags::NEWNS)?;
            // Fails with EINVAL once no mount is left; bounded in case of a
            // different error.
            for _ in 0..16 {
                if libc::umount2(proc_net.as_ptr(), libc::MNT_DETACH) != 0 {
                    break;
                }
            }
            let fd = BorrowedFd::borrow_raw(fd);
            move_into_link_name_space(fd, Some(LinkNameSpaceType::User))?;
            crate::netns::unshare_namespaces(UnshareFlags::NEWNS)?;
            Ok(())
        });
    }
    Ok(cmd)
}

/// Runs a podman subcommand from the blocking pool and returns its output.
///
/// The command runs in the caller's (host) network namespace. Use
/// [`podman_in`] for commands that must run in a device.
pub(crate) async fn podman(args: Vec<String>) -> Result<std::process::Output> {
    tokio::task::spawn_blocking(move || podman_blocking(&args))
        .await
        .context("container runtime task panicked")?
}

/// Blocking core of [`podman`].
fn podman_blocking(args: &[String]) -> Result<std::process::Output> {
    runtime_command()?
        .args(args)
        .output()
        .context("spawn `podman` (is it installed?)")
}

/// Runs a podman subcommand from inside a device's network namespace and
/// returns its output.
///
/// `podman run --network=host` and `podman exec` of such a container use the
/// network namespace of the podman process itself.
async fn podman_in(device: &Device, args: Vec<String>) -> Result<std::process::Output> {
    let mut cmd = tokio::process::Command::from(runtime_command()?);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    device
        .spawn_command(cmd)
        .context("spawn `podman` (is it installed?)")?
        .wait_with_output()
        .await
        .context("wait for podman")
}

/// Returns `<pid>:<start time>` of the current process, the value of
/// [`OWNER_LABEL`]. The start time tells a reused pid apart.
///
/// If the start time cannot be read, the tag says `unknown`, and
/// [`owner_alive`] treats such a tag as alive, so its containers are never
/// reaped.
fn owner_tag() -> String {
    static TAG: OnceLock<String> = OnceLock::new();
    TAG.get_or_init(|| {
        let pid = std::process::id();
        match process_start_time(pid) {
            Some(start) => format!("{pid}:{start}"),
            None => format!("{pid}:unknown"),
        }
    })
    .clone()
}

/// Returns the start time of a process (field 22 of `/proc/<pid>/stat`).
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name in field 2 may contain spaces; skip past it.
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Returns whether the process in an [`OWNER_LABEL`] value may still run.
///
/// A tag that does not parse counts as alive, so a container is only reaped
/// when its owner is known to be gone.
fn owner_alive(tag: &str) -> bool {
    let parsed = tag
        .split_once(':')
        .and_then(|(pid, start)| Some((pid.parse::<u32>().ok()?, start.parse::<u64>().ok()?)));
    match parsed {
        Some((pid, start)) => process_start_time(pid) == Some(start),
        None => true,
    }
}

/// A container whose owning patchbay process is gone.
#[derive(Debug, PartialEq, Eq)]
struct Orphan {
    name: String,
    /// Host pid of the container's first process, 0 if it does not run.
    pid: u32,
}

/// Returns the containers in `podman ps --format json` output whose owner no
/// longer runs.
fn orphans(json: &[u8]) -> Result<Vec<Orphan>> {
    let containers: Vec<serde_json::Value> = serde_json::from_slice(json)?;
    Ok(containers
        .iter()
        .filter(|c| {
            c["Labels"][OWNER_LABEL]
                .as_str()
                .is_some_and(|owner| !owner_alive(owner))
        })
        .filter_map(|c| {
            Some(Orphan {
                name: c["Names"][0].as_str()?.to_owned(),
                pid: c["Pid"]
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())
                    .unwrap_or(0),
            })
        })
        .collect())
}

/// Runs [`reap_orphans`] once per process.
async fn reap_orphans_once() {
    static REAPED: OnceCell<()> = OnceCell::const_new();
    REAPED.get_or_init(reap_orphans).await;
}

/// Removes containers whose owning patchbay process is gone.
///
/// A process that exits without unwinding (`SIGKILL`, `abort`, Ctrl-C) never
/// drops its [`Container`] handles. Its containers keep running and hold on to
/// their device's network namespace. Containers of live processes, for example
/// a parallel test binary, are left alone.
///
/// Each patchbay process runs podman in its own user namespace, which has no
/// say over another process's containers. A container whose first process
/// runs as root inside maps to the user and can be stopped anyway, but one
/// that runs as another user cannot. So the first process is killed through
/// [`host_kill`](crate::userns::host_kill) from the initial user namespace
/// before `podman rm`. Killing it ends every process of the container.
pub(crate) async fn reap_orphans() {
    let filter = format!("label={OWNER_LABEL}");
    let args = ["ps", "-a", "--filter", &filter, "--format", "json"].map(String::from);
    let output = match podman(args.to_vec()).await {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            warn!(stderr = %String::from_utf8_lossy(&output.stderr).trim(), "listing containers failed");
            return;
        }
        Err(err) => {
            warn!(%err, "listing containers failed");
            return;
        }
    };
    let orphans = match orphans(&output.stdout) {
        Ok(orphans) => orphans,
        Err(err) => {
            warn!(%err, "parsing the container list failed");
            return;
        }
    };
    if orphans.is_empty() {
        return;
    }
    debug!(?orphans, "removing containers of exited processes");
    for orphan in &orphans {
        if orphan.pid > 0 {
            if let Err(err) = crate::userns::host_kill(orphan.pid, libc::SIGKILL) {
                debug!(container = %orphan.name, %err, "could not kill the container's first process");
            }
        }
    }
    let mut args = ["rm", "-f", "-t", "0"].map(String::from).to_vec();
    args.extend(orphans.iter().map(|orphan| orphan.name.clone()));
    match podman(args).await {
        Ok(output) if output.status.success() => {}
        Ok(output) => warn!(
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "could not remove containers of exited processes"
        ),
        Err(err) => warn!(%err, "could not remove containers of exited processes"),
    }
}

/// Ensures the image is present locally, pulling it on the host (which has
/// registry access) if needed.
///
/// The container itself runs in a device namespace with no route to a registry,
/// so the image must be present before that run.
async fn ensure_image(image: &str) -> Result<()> {
    if image_present(image).await {
        return Ok(());
    }
    let output = podman(vec!["pull".into(), image.into()]).await?;
    if !output.status.success() {
        bail!(
            "pulling image '{image}' failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Returns whether the image already exists in local storage.
async fn image_present(image: &str) -> bool {
    podman(vec!["image".into(), "exists".into(), image.into()])
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Shared interior of a [`Container`], removed when the last handle drops.
struct ContainerInner {
    name: String,
}

impl Drop for ContainerInner {
    fn drop(&mut self) {
        // Best-effort removal by name, which works whether or not the device
        // namespace is still alive. Inside a Tokio runtime, remove from the
        // blocking pool so the drop does not stall the runtime. The runtime
        // waits for blocking tasks when it shuts down.
        let name = std::mem::take(&mut self.name);
        let remove = move || {
            let args = ["rm", "-f", "-t", "1", &name].map(String::from);
            match podman_blocking(&args) {
                Ok(out) if out.status.success() => {}
                Ok(out) => warn!(
                    container = %name,
                    stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "container removal reported an error"
                ),
                Err(err) => warn!(container = %name, %err, "could not run container removal"),
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => drop(handle.spawn_blocking(remove)),
            Err(_) => remove(),
        }
    }
}

/// A container running as a lab device.
///
/// A container is a [`Device`] whose workload is an OCI image instead of a
/// Rust closure. It is wired into the lab like a device (its own network
/// namespace, a veth uplink to a router, a lab IP), and the container joins
/// that namespace, so other lab devices reach it at its lab IP. Create one
/// with [`Lab::add_container`](crate::Lab::add_container).
///
/// Cloneable handle. Dereferences to the underlying [`Device`], so device
/// accessors ([`ip`](Device::ip), [`spawn`](Device::spawn), ...) work directly.
/// The container is removed when the last handle is dropped, or explicitly via
/// [`stop`](Self::stop). Containers of a process that died without cleanup,
/// for example on `SIGKILL` or Ctrl-C, keep running until the next patchbay
/// process that starts a container removes them.
///
/// # Runtime
///
/// Containers run with `podman`, which must be on `PATH`. podman is
/// daemonless: the `podman run` process is forked from inside the device's
/// network namespace, so `--network=host` makes the container join that
/// namespace directly. Docker does not work, because its daemon runs in a
/// different namespace, so a docker container would not land in the device's
/// namespace.
///
/// # Requirements
///
/// The host needs the standard rootless-podman setup: `newuidmap` and
/// `newgidmap` on `PATH`, and an `/etc/subuid` and `/etc/subgid` entry for the
/// user. [`init_userns`](crate::init_userns) then maps that range. Without it,
/// [`ContainerBuilder::build`] fails with an error that says so. The process
/// keeps its own uid either way, so it can use the session D-Bus or PipeWire
/// portals and run containers.
///
/// # Differences from a device
///
/// - The [`Device`] methods that run code, such as [`run_sync`](Device::run_sync),
///   [`spawn`](Device::spawn) and [`spawn_command`](Device::spawn_command), run
///   it in the device's network namespace on the host file system, not in the
///   container. Use [`exec`](Self::exec) to run a command in the container.
/// - The container gets a copy of the device's `/etc/resolv.conf` when it
///   starts, so the lab [`DnsServer`](crate::DnsServer) works if it exists at
///   that point. Entries added with [`Device::set_host`] do not reach the
///   container's `/etc/hosts`.
/// - Container root has no capabilities in the lab's network namespaces, so a
///   container cannot change routes or firewall rules. It can bind low ports
///   and ping.
/// - [`Lab::remove_device`](crate::Lab::remove_device) does not stop the
///   container. Drop the handle or call [`stop`](Self::stop).
/// - podman in your shell lists patchbay's containers and can remove stopped
///   ones, but cannot `exec` into them, copy files, or stop one whose processes
///   run as a user other than root, because they run in patchbay's user
///   namespace.
///
/// # Example
///
/// ```no_run
/// # use patchbay::Lab;
/// # async fn f(lab: Lab, net_id: patchbay::NodeId) -> anyhow::Result<()> {
/// let pebble = lab
///     .add_container("pebble", "ghcr.io/letsencrypt/pebble:latest")
///     .uplink(net_id)
///     .env("PEBBLE_VA_NOSLEEP", "1")
///     .ready_tcp(14000)
///     .build()
///     .await?;
///
/// // Reachable from any lab device on `pebble.ip()`.
/// let directory = format!(
///     "https://{}:14000/dir",
///     pebble.ip().expect("pebble has an ip")
/// );
/// # let _ = directory;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Container {
    device: Device,
    inner: Arc<ContainerInner>,
}

impl std::fmt::Debug for Container {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Container")
            .field("name", &self.inner.name)
            .field("ip", &self.device.ip())
            .finish()
    }
}

impl Deref for Container {
    type Target = Device;
    fn deref(&self) -> &Device {
        &self.device
    }
}

impl Container {
    /// Returns the underlying [`Device`] handle.
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Returns the podman container name (unique on the host).
    pub fn container_name(&self) -> &str {
        &self.inner.name
    }

    /// Runs a command inside the running container and returns its output.
    ///
    /// The command runs in the container's file system and in the device's
    /// network namespace, like the container's own processes.
    ///
    /// # Errors
    ///
    /// Returns an error if podman cannot be spawned. A non-zero exit from the
    /// command itself is reported in the returned [`std::process::Output`],
    /// not as an error.
    pub async fn exec<I, S>(&self, cmd: I) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut args = vec!["exec".to_string(), self.inner.name.clone()];
        args.extend(cmd.into_iter().map(Into::into));
        podman_in(&self.device, args).await
    }

    /// Returns the container's logs (stdout and stderr, combined).
    pub async fn logs(&self) -> Result<String> {
        let output = podman(vec!["logs".into(), self.inner.name.clone()]).await?;
        if !output.status.success() {
            bail!(
                "reading the logs of container '{}' failed: {}",
                self.inner.name,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let mut logs = String::from_utf8_lossy(&output.stdout).into_owned();
        logs.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(logs)
    }

    /// Stops and removes the container.
    ///
    /// Called automatically when the last handle drops; use this to tear it
    /// down early or to surface removal errors.
    pub async fn stop(&self) -> Result<()> {
        let args = vec![
            "rm".into(),
            "-f".into(),
            "-t".into(),
            "1".into(),
            self.inner.name.clone(),
        ];
        let output = podman(args).await?;
        if !output.status.success() {
            bail!(
                "removing container '{}' failed: {}",
                self.inner.name,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Copies a host file or directory into the container.
    ///
    /// The destination's parent directory must already exist in the container.
    pub async fn copy_to(&self, host_src: impl AsRef<Path>, container_dest: &str) -> Result<()> {
        let src = host_src.as_ref().to_string_lossy().into_owned();
        let dest = format!("{}:{}", self.inner.name, container_dest);
        let output = podman(vec!["cp".into(), src, dest]).await?;
        if !output.status.success() {
            bail!(
                "copy into container '{}' failed: {}",
                self.inner.name,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Copies a file or directory out of the container to a host path.
    pub async fn copy_from(&self, container_src: &str, host_dest: impl AsRef<Path>) -> Result<()> {
        let src = format!("{}:{}", self.inner.name, container_src);
        let dest = host_dest.as_ref().to_string_lossy().into_owned();
        let output = podman(vec!["cp".into(), src, dest]).await?;
        if !output.status.success() {
            bail!(
                "copy out of container '{}' failed: {}",
                self.inner.name,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Writes bytes to a file inside the container, creating or replacing it.
    ///
    /// Accepts anything byte-like (`&str`, `String`, `&[u8]`, ...). The parent
    /// directory must already exist in the container.
    pub async fn write_file(&self, container_path: &str, contents: impl AsRef<[u8]>) -> Result<()> {
        let dir = tempfile::tempdir().context("create staging directory")?;
        let staged = dir.path().join("file");
        tokio::fs::write(&staged, contents.as_ref())
            .await
            .context("write staging file")?;
        self.copy_to(&staged, container_path).await
    }

    /// Reads a file from inside the container.
    pub async fn read_file(&self, container_path: &str) -> Result<Vec<u8>> {
        let dir = tempfile::tempdir().context("create staging directory")?;
        let staged = dir.path().join("file");
        self.copy_from(container_path, &staged).await?;
        tokio::fs::read(&staged)
            .await
            .with_context(|| format!("read '{container_path}', which must be a file"))
    }

    /// Returns whether the container's processes still run.
    async fn is_running(&self) -> Result<bool> {
        let args = [
            "inspect",
            "--format",
            "{{.State.Running}}",
            &self.inner.name,
        ];
        let output = podman(args.map(String::from).to_vec()).await?;
        anyhow::ensure!(
            output.status.success(),
            "inspecting container '{}' failed: {}",
            self.inner.name,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&output.stdout).trim() == "true")
    }

    /// Polls the readiness gate until it passes or times out.
    async fn wait_ready(&self, ready: &Ready) -> Result<()> {
        let Ready::Tcp { port, timeout } = ready else {
            return Ok(());
        };
        let ip = self
            .device
            .ip()
            .map(IpAddr::V4)
            .or_else(|| self.device.ip6().map(IpAddr::V6))
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let target = SocketAddr::new(ip, *port);
        let deadline = Instant::now() + *timeout;
        let mut next_state_check = Instant::now() + Duration::from_secs(1);
        loop {
            let connected = self
                .device
                .spawn(move |_| async move {
                    let connect = tokio::net::TcpStream::connect(target);
                    matches!(
                        tokio::time::timeout(Duration::from_millis(500), connect).await,
                        Ok(Ok(_))
                    )
                })?
                .await
                .context("readiness task panicked")?;
            if connected {
                debug!(container = %self.inner.name, %target, "container ready");
                return Ok(());
            }
            let exited = if Instant::now() >= next_state_check {
                next_state_check = Instant::now() + Duration::from_secs(1);
                !self.is_running().await?
            } else {
                false
            };
            if exited || Instant::now() >= deadline {
                let logs = self.logs().await.unwrap_or_default();
                let why = if exited {
                    "exited before it accepted a connection".to_string()
                } else {
                    format!("did not accept a connection within {timeout:?}")
                };
                bail!(
                    "container '{}' on {target} {why}\n--- logs ---\n{}",
                    self.inner.name,
                    logs.trim()
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_args_have_the_required_flags() {
        let args = build_run_args(
            "lab-p1-c0",
            "docker.io/library/alpine",
            &[("A".into(), "1".into()), ("B".into(), "2".into())],
            &[],
            &["sleep".into(), "infinity".into()],
            &["--cgroup-manager=cgroupfs".into()],
        );
        // Detached, name, host network, and no --replace, which could remove
        // another process's container.
        assert_eq!(&args[0], "run");
        assert!(args.contains(&"-d".to_string()));
        assert!(args.contains(&"--network=host".to_string()));
        assert!(!args.contains(&"--replace".to_string()));
        let name_idx = args.iter().position(|a| a == "--name").unwrap();
        assert_eq!(args[name_idx + 1], "lab-p1-c0");
        let owner = format!("{OWNER_LABEL}={}", owner_tag());
        assert!(args.windows(2).any(|w| w[0] == "--label" && w[1] == owner));
        // Env pairs.
        assert!(args.windows(2).any(|w| w[0] == "--env" && w[1] == "A=1"));
        assert!(args.windows(2).any(|w| w[0] == "--env" && w[1] == "B=2"));
        // Extra run arg comes before the image.
        let image_idx = args
            .iter()
            .position(|a| a == "docker.io/library/alpine")
            .unwrap();
        let cgroup_idx = args
            .iter()
            .position(|a| a == "--cgroup-manager=cgroupfs")
            .unwrap();
        assert!(cgroup_idx < image_idx);
        // Command args come after the image, in order.
        assert_eq!(&args[image_idx + 1], "sleep");
        assert_eq!(&args[image_idx + 2], "infinity");
    }

    #[test]
    fn mounts_render_before_the_image() {
        let mounts = vec![
            Mount::Bind {
                host: "/srv/data".into(),
                container: "/data".into(),
                read_only: false,
            },
            Mount::Bind {
                host: "/etc/conf".into(),
                container: "/conf".into(),
                read_only: true,
            },
            Mount::Tmpfs {
                container: "/scratch".into(),
            },
        ];
        let args = build_run_args("c", "img", &[], &mounts, &[], &[]);
        let image_idx = args.iter().position(|a| a == "img").unwrap();
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--volume" && w[1] == "/srv/data:/data"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--volume" && w[1] == "/etc/conf:/conf:ro"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--tmpfs" && w[1] == "/scratch"));
        // All mount args precede the image.
        let last_mount = args
            .iter()
            .rposition(|a| a == "--volume" || a == "--tmpfs")
            .unwrap();
        assert!(last_mount < image_idx);
    }

    fn line(inner: u32, outer: u32, count: u32) -> IdMapLine {
        IdMapLine {
            inner,
            outer,
            count,
        }
    }

    #[test]
    fn parse_id_map_reads_padded_lines() {
        let map =
            parse_id_map("         0       1000          1\n         1     100000      65536\n")
                .unwrap();
        assert_eq!(map, [line(0, 1000, 1), line(1, 100000, 65536)]);
        assert!(parse_id_map("0 1000\n").is_err());
    }

    #[test]
    fn nested_map_matches_rootless_podman_layout() {
        // patchbay's map for uid 1000 with subordinate ids 100000..=165535.
        let map = [
            line(1000, 1000, 1),
            line(0, 100000, 1000),
            line(1001, 101000, 64536),
        ];
        let (lines, host) = nested_id_map(&map, 1000).unwrap();
        // Nested N maps to host 100000 + N - 1, as in plain rootless podman.
        assert_eq!(lines, "0 1000 1\n1 0 1000\n1001 1001 64536\n");
        assert_eq!(host, 1000);
    }

    #[test]
    fn nested_map_sorts_by_host_id() {
        let map = [line(5, 200, 10), line(1000, 1000, 1), line(0, 100, 5)];
        let (lines, _) = nested_id_map(&map, 1000).unwrap();
        assert_eq!(lines, "0 1000 1\n1 0 5\n6 5 10\n");
    }

    #[test]
    fn nested_map_needs_a_range() {
        let err = nested_id_map(&[line(1000, 1000, 1)], 1000).unwrap_err();
        assert!(err.to_string().contains("subordinate id range"), "{err}");
        assert!(nested_id_map(&[line(1000, 1000, 1)], 5).is_err());
    }

    #[test]
    fn ping_range_spans_all_mapped_host_gids() {
        let map = [
            line(1000, 1000, 1),
            line(0, 100000, 1000),
            line(1001, 101000, 64536),
        ];
        assert_eq!(ping_group_range(&map).unwrap(), (1000, 65536));
        let single = [line(1000, 1000, 1)];
        assert_eq!(ping_group_range(&single).unwrap(), (1000, 1000));
        // A range smaller than the gid: inner order disagrees with host order.
        let small = [line(1000, 1000, 1), line(0, 100000, 10)];
        assert_eq!(ping_group_range(&small).unwrap(), (1000, 1000));
    }

    #[test]
    fn owner_tag_of_this_process_is_alive() {
        assert!(owner_alive(&owner_tag()));
        let (pid, start) = owner_tag()
            .split_once(':')
            .map(|(p, s)| (p.to_owned(), s.to_owned()))
            .unwrap();
        assert!(!owner_alive(&format!(
            "{pid}:{}",
            start.parse::<u64>().unwrap() + 1
        )));
        // Tags that do not parse count as alive, so they are never reaped.
        assert!(owner_alive("garbage"));
        assert!(owner_alive("12:unknown"));
    }

    #[test]
    fn orphans_are_containers_of_dead_owners() {
        let json = format!(
            r#"[
                {{"Names": ["alive"], "Labels": {{"{OWNER_LABEL}": "{}"}}, "Pid": 5}},
                {{"Names": ["dead"], "Labels": {{"{OWNER_LABEL}": "1:1"}}, "Pid": 42}},
                {{"Names": ["exited"], "Labels": {{"{OWNER_LABEL}": "1:1"}}, "Pid": 0}},
                {{"Names": ["unlabelled"], "Labels": null}}
            ]"#,
            owner_tag()
        );
        assert_eq!(
            orphans(json.as_bytes()).unwrap(),
            [
                Orphan {
                    name: "dead".into(),
                    pid: 42
                },
                Orphan {
                    name: "exited".into(),
                    pid: 0
                }
            ]
        );
    }

    #[test]
    fn sanitize_name_replaces_invalid_chars() {
        assert_eq!(sanitize_name("lab-p1/c0"), "lab-p1-c0");
        assert_eq!(sanitize_name("ok_name.1-2"), "ok_name.1-2");
        assert_eq!(sanitize_name("a b:c"), "a-b-c");
    }
}
