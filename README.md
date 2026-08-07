# minimalbase

A minimal OCI base image plus `container-init`, a small PID 1 for shell-less
containers. Init reads a read-only JSON contract at `/app/main`, spawns the
payload, forwards `SIGTERM`/`SIGINT` to it, and reaps orphans.

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

## Crash-loop rate limiting

If the payload exits sooner than 120s after boot, init sleeps out the remainder
before exiting, so a container that fails instantly cannot spin a restart loop.
