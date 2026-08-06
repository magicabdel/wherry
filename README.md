<!--
HERO IMAGE
The banner lives at docs/assets/hero.jpg.
-->
<p align="center">
  <img src="docs/assets/hero.png" alt="wherry" width="640">
</p>

# wherry ⛵

> A *wherry* is a light boat used to ferry people out to a larger ship — which
> is exactly what this tool does: it's your little craft for hopping aboard
> any EKS cluster.

One small Rust binary that wires `kubectl` up to your EKS clusters —
including **private** ones. It configures `~/.kube/config` so that a SOCKS
bridge through a bastion spins up automatically every time you run `kubectl`.

No Python runtime, no SSH client: key generation, the SSH tunnel and the
SOCKS5 proxy are all native Rust. The only requirement is the
[AWS CLI](https://docs.aws.amazon.com/cli/) with a configured profile.

## Install

With the install script (Linux & macOS):

```sh
wget -qO- https://raw.githubusercontent.com/magicabdel/wherry/main/install.sh | sh
# or
curl -fsSL https://raw.githubusercontent.com/magicabdel/wherry/main/install.sh | sh
```

Or with Cargo:

```sh
cargo install wherry
```

## Quick start

```sh
wherry update-kubeconfig
```

That's it — pick a profile, pick a cluster, answer the bastion prompt, and
`kubectl get pod` just works. The bridge starts on demand and expired AWS SSO
tokens are refreshed automatically.

Prefer to be explicit? Every prompt has a flag:

```sh
wherry update-kubeconfig eks-1234 \
  --profile profile-1234 --region eu-central-1 --alias test-cluster
```

| Flag | Meaning |
| --- | --- |
| `--profile` | AWS profile (otherwise picked from a list) |
| `--region` | defaults to `eu-west-1` |
| `--alias` | friendlier context name (otherwise prompted, Enter keeps the cluster name) |
| `--port` | local SOCKS port (defaults to a random free port) |
| `--bastion <name>` / `--no-bastion` | pin a bastion, or skip it entirely |

## Other commands

```sh
wherry bridge start        # open a bastion bridge by hand
wherry bridge stop [--all] # tear bridges down (they run as daemons)
wherry sso login           # refresh an AWS SSO token manually
wherry sso status          # freshness of every configured session/profile
wherry update              # update wherry itself to the latest release
```

Everything is interactive when arguments are omitted; `wherry --help` shows
the rest.

## How it works

- **Bridge** — wherry finds the load balancer tagged `usage=bastion`,
  generates an ed25519 key, pushes it via EC2 Instance Connect, then serves a
  local SOCKS5 proxy over a native SSH connection. It runs as a background
  daemon (PID file in `$TMPDIR`, logs in `$TMPDIR/wherry-bridge.log`) so the
  kubeconfig `exec` plugin can start it transparently.
- **SSO** — refresh is purely reactive: the real AWS call runs first, and only
  if it fails with an expired/missing cached token does wherry run
  `aws sso login` and retry once. A healthy token never costs anything.
  `sso_session`, legacy `sso_start_url` and `source_profile` chains are all
  followed.
- **Credentials** — resolution is delegated to the AWS SDK, so
  `~/.aws/config` and `~/.aws/credentials` are read exactly like the AWS CLI
  reads them.

## Development

```sh
cargo build --release   # binary at target/release/wherry
```

```
src/
  main.rs         CLI definition (clap)
  commands/       command handlers (kubeconfig, bridge, sso)
  aws/            AWS SDK wrappers (session, profiles, eks, bastion, sso)
  kube_config.rs  reads/edits ~/.kube/config
  ssh.rs          ed25519 key generation
  bridge.rs       SSH + SOCKS5 bridge + daemonization
  prompt.rs       interactive selection helpers
```

Releases are built by CI for Linux (x86_64, aarch64, static musl) and macOS
(Intel, Apple Silicon) whenever a `v*` tag is pushed.
