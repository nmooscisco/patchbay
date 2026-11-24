# Running Containers

Some integration tests need auxiliary services that are easier to run as a
container than to embed: an ACME test server like [Pebble], a database, a
message broker, a real DNS server. patchbay can run these as ordinary lab
nodes.

A **container is a device**. It gets its own network namespace, a veth uplink
to a router, and a lab IP, exactly like a device created with
[`add_device`](https://docs.rs/patchbay/latest/patchbay/struct.Lab.html#method.add_device).
The difference is only what runs inside it: an OCI image instead of a Rust
closure. Other lab devices reach the service at the container's lab IP.

## Example

```rust,ignore
let net = lab.add_router("net").build().await?;

// Start Pebble as a lab device on `net`, and wait until it is listening.
let pebble = lab
    .add_container("pebble", "ghcr.io/letsencrypt/pebble:latest")
    .uplink(net.id())
    .env("PEBBLE_VA_NOSLEEP", "1")
    .ready_tcp(14000)
    .build()
    .await?;

// The directory URL other lab devices use.
let directory = format!("https://{}:14000/dir", pebble.ip().unwrap());

// A client device on the same network can now reach it.
let client = lab.add_device("client").uplink(net.id()).build().await?;
```

The builder mirrors the device builder for wiring (`uplink`, `iface`,
`default_via`, `mtu`) and adds container configuration:

| Method | Purpose |
|--------|---------|
| `env(k, v)` | Set an environment variable in the container. |
| `arg(a)` / `args([..])` | Append to the container's command. |
| `volume(host, container)` | Bind-mount a host path read-write. |
| `volume_ro(host, container)` | Bind-mount a host path read-only. |
| `tmpfs(container)` | Mount a fresh tmpfs at a path. |
| `run_arg(a)` | Pass an extra flag to `podman run` (escape hatch). |
| `ready_tcp(port)` | Block `build` until `port` accepts a connection (30 s timeout). |
| `ready_tcp_timeout(port, timeout)` | Like `ready_tcp` with an explicit timeout. |

## Volumes and config

Hand a service its config or data with a bind mount. Host paths should be
absolute:

```rust,ignore
let pebble = lab
    .add_container("pebble", "ghcr.io/letsencrypt/pebble:latest")
    .uplink(net.id())
    .volume_ro(config_dir.join("pebble.json"), "/test/config/pebble.json")
    .args(["-config", "/test/config/pebble.json"])
    .ready_tcp(14000)
    .build()
    .await?;
```

On SELinux hosts a bind mount may need relabeling or no label. The builder
cannot add a `:z` suffix to a volume, but you can turn labeling off with
`run_arg("--security-opt=label=disable")`.

The returned [`Container`] dereferences to [`Device`], so `ip()`, `run_sync()`,
and the other device accessors work directly. It also has:

- `exec(cmd)` runs a command inside the container.
- `logs()` fetches the container's logs.
- `write_file(path, bytes)` and `read_file(path)` write or read a file inside
  the container without a bind mount.
- `copy_to(host, container)` and `copy_from(container, host)` copy files or
  directories in or out.
- `stop()` removes the container early. Dropping the last handle also
  removes it.

If a process dies without cleanup (for example on `SIGKILL` or Ctrl-C), its
containers keep running. The next patchbay process that starts a container
removes them. Each container has a `patchbay.owner` label with the pid and
start time of the process that started it. podman in your shell lists these
containers and can remove stopped ones, but cannot `exec` into them, copy
files, or stop one that runs as a user other than root, because they run in
patchbay's user namespace. `kill` on the container's process works.

## Differences from a device

- The container gets a copy of the device's `/etc/resolv.conf` when it
  starts. The lab DNS server works if it exists at that point. Entries added
  with `Device::set_host` do not reach the container's `/etc/hosts`.
- Root in the container has no capabilities in the lab's network
  namespaces, so a container cannot change routes or firewall rules. patchbay
  sets per-namespace sysctls so that it can still bind ports below 1024 and
  ping.
- `run_sync`, `spawn` and the other device methods run code in the device's
  network namespace on the host file system, not in the container. Use
  `exec` to run a command in the container.
- `Lab::remove_device` does not stop the container. Drop the handle or call
  `stop()`.

## Runtime: podman, not docker

Containers run with **podman**, which must be on `PATH`. This is not
incidental. patchbay is rootless: the whole process lives in an unprivileged
user namespace, and each node's network namespace is entered by a worker thread,
not persisted to a path. podman is daemonless, so the `podman run` process,
forked from inside the device's worker thread, is already in the device's
network namespace; `--network=host` then means "use the namespace I am already
in", and the container lands on the device's lab IP with no extra plumbing.

Docker cannot do this rootlessly. Its client talks to `dockerd`, which runs in
its own namespace, so a docker container joins the daemon's namespace, not the
device's. Making docker work would require manipulating the container's
namespace from outside its owning user namespace, which rootless patchbay
cannot do.

## Requirements

Running a container needs the standard rootless-podman setup:

- `podman`, `newuidmap`, and `newgidmap` on `PATH`.
- An `/etc/subuid` and `/etc/subgid` entry for your user (for example
  `you:100000:65536`).

`init_userns()` maps that range with `newuidmap`/`newgidmap` when it
bootstraps the user namespace. Without it, `build()` fails with an error that
says so. Router and device networking work regardless. The process keeps its
own uid, so it can use the session D-Bus or PipeWire portals while it runs
containers.

podman runs in a user namespace nested in patchbay's, laid out like podman's
own rootless namespace: root in it is your user, and the other ids are your
subordinate range. podman therefore uses your normal rootless storage, and
images you pulled with podman before are available.

The image is pulled on the host before the container starts, because the device
namespace it runs in has no route to a registry. If a specific image needs a
runtime tweak, pass it through with `run_arg`, for example
`run_arg("--cgroup-manager=cgroupfs")`.

[Pebble]: https://github.com/letsencrypt/pebble
[`Container`]: https://docs.rs/patchbay/latest/patchbay/struct.Container.html
[`Device`]: https://docs.rs/patchbay/latest/patchbay/struct.Device.html
