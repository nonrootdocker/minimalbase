# minimalbase

A minimal OCI base image plus `container-init`, a small PID 1 for shell-less
containers. Init reads a read-only JSON contract at `/app/main`, spawns the
payload, forwards `SIGTERM`/`SIGINT` to it, and reaps orphans. It can either run
the payload once, or idle and run it on demand for cron-driven work.

## The ABI

`/app/main` is baked into the image and owned by root:

```json
{
  "process": {
    "exec": "/bin/app",
    "args": ["-data=/data"],
    "cwd": "/data",
    "dirs": ["/data/config", "/data/cache"]
  }
}
```

| Field  | Required | Meaning |
| ------ | -------- | ------- |
| `exec` | yes      | Absolute path, or a bare name resolved via `$PATH`. |
| `args` | no       | Baseline arguments. Runtime arguments are appended to these. |
| `cwd`  | no       | `chdir` here before exec. |
| `dirs` | no       | `mkdir -p` each of these before exec. |
| `mode` | no       | `oneshot` (default) or `triggered`. See [Run modes](#run-modes). |

## Passing arguments at runtime

Init runs as the image's entrypoint, so a Compose `command:` (equivalently, a
`docker run` trailing command) arrives as init's own argv. Those words are
**appended** to the ABI's `args` and handed to the payload:

```yaml
services:
  app:
    image: ghcr.io/nonrootdocker/myservice
    command: ["--port=8080"]
```

With the ABI above, the payload is exec'd as `/bin/app -data=/data --port=8080`.

Appending rather than replacing means an image declaring `"args": []` yields
full control to the deployment, while an image with a baseline keeps the flags
it needs — a Compose file cannot accidentally drop `-data=/data` and send the
payload at the wrong volume. Note that overriding a baseline flag by repeating
it only works if the payload itself is last-wins; adding new flags always works.

`exec` is never runtime-settable. It stays pinned by the read-only ABI, so the
container's argv is extensible but the binary it runs is not.

Downstream images must set the entrypoint themselves:

```nix
config = {
  Entrypoint = [ "${minimalbase.packages.${system}.container-init}/bin/container-init" ];
};
```

## Run modes

### `oneshot` (default)

Init runs the payload once and exits when it does — a conventional entrypoint.

### `triggered`

Init stays resident and idle, running the payload once per `SIGUSR1`. Transient
work — a backup, a sync, a report — otherwise leaves a stopped container behind,
which fights `restart: unless-stopped` and gets swept up by anything that prunes
stopped containers. In `triggered` mode the container is legitimately up because
it is meant to be up, and an external scheduler decides when work happens:

```json
{
  "process": {
    "exec": "/bin/backup",
    "args": ["-data=/data"],
    "mode": "triggered"
  }
}
```

```crontab
# on the host
0 3 * * *  docker kill -s SIGUSR1 backup
```

```
[init] idle; waiting for SIGUSR1 to run /bin/backup
[init] trigger received; started payload (pid 14)
[init] payload exited (exit code 0)
[init] idle; waiting for SIGUSR1
```

Semantics:

* Nothing runs at boot. A host reboot or redeploy never causes an unscheduled
  run; only a trigger does.
* Triggers do not queue or overlap. One arriving while a run is in flight is
  logged and dropped, so two copies never race on the same data.
* A payload that fails does not stop init — it logs the exit status and returns
  to idle, ready for the next trigger.
* `SIGTERM`/`SIGINT` is forwarded to any in-flight run, and init exits once that
  run has finished.
* The payload's exit status reaches the log, not `docker inspect`; the container
  is a scheduler target, so its own exit code reports on init, not on the job.

`docker kill` only sends a signal — despite the name it does not stop the
container. Podman uses the same flag.

## Crash-loop rate limiting

If the payload exits sooner than 120s after boot, init sleeps out the remainder
before exiting, so a container that fails instantly cannot spin a restart loop.

This applies to `oneshot` and to fatal ABI errors in either mode. It is skipped
when a `triggered` container shuts down, since that only happens on request and
stalling it would push `docker stop` into its `SIGKILL` timeout.
