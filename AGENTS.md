# gcs (google-cloud-shell-cli) — Agent Guide

## Overview

Rust CLI for Google Cloud Shell without the gcloud SDK. It signs in like gcloud,
keeps credentials in gcloud's ADC file, calls the Cloud Shell API directly, and
runs the system `ssh`/`scp` through a WebSocket tunnel it provides itself as
the `ProxyCommand`.

## Repository Structure

```text
src/
├── main.rs     # clap CLI, dispatch, ssh/scp/ssh-proxy/info/start commands
├── creds.rs    # credential source precedence, ADC parsing, quota project
├── oauth.rs    # sign-in (loopback + paste-code), refresh, revoke, tokeninfo
├── auth.rs     # `gcs auth login|logout|status|token`, ADC file writing
├── api.rs      # Cloud Shell API v1: get/start/authorize/generateAccessToken
├── tunnel.rs   # stdin/stdout <-> wss://<webHost>/_cloudshell/tcp/<port>
└── ssh.rs      # ssh/scp argument building, ProxyCommand string, cs: paths
skills/
└── gcs/SKILL.md  # agent-facing skill (source of truth, see below)
```

## Agent Skill

`skills/gcs/SKILL.md` is the **source of truth** for the agent-facing skill: it
teaches an agent (Claude Code, opencode, ...) when to use gcs, which steps
need the human (sign-in), and how to handle each error. Copies installed into
an agent's skills directory (e.g. `~/.claude/skills/gcs/`) are deployed
artifacts. Edit the file here, then re-sync, rather than editing an installed
copy.

When an error message, flag, default or exit code changes, update the skill's
command reference and failure-handling table in the same change. Agents match
on the quoted error text.

## Key Concepts

### Credential sources

`creds::choose` is a pure function (flags + injected env + default path) so
precedence is unit-tested. Order: one credential flag, then `$GCS_ACCESS_TOKEN`,
`$GCS_ACCESS_TOKEN_FILE`, `$GOOGLE_APPLICATION_CREDENTIALS`, then gcloud's
default ADC path (`$CLOUDSDK_CONFIG` or `~/.config/gcloud`, `%APPDATA%\gcloud`
on Windows). Two credential flags at once is an error, checked in
`CredFlags::ensure_single` because clap argument groups miss global flags given
after the subcommand. Empty env values count as unset.

### Which OAuth client

`auth login` uses gcloud CLI's client (`32555940559…`), the one
`gcloud auth login --update-adc` writes into ADC. Not gcloud's ADC client
(`764086051850…`): verified live, the Cloud Shell API answers that one with
403 "requires a quota project". ADC files from
`gcloud auth application-default login` therefore need `--quota-project` /
`$GCS_QUOTA_PROJECT` (or `quota_project_id` in the file); the API error path
says so. Login writes no quota project, as gcloud does, because a stale one
can make the API refuse requests.

The ADC file layout must stay byte-compatible with gcloud's (sorted keys,
two-space indent, `"account": ""`, `0600`); `auth::tests` pins it.

### SSH path

1. `api.ensure_running` gets the environment and starts it if needed.
2. `ssh`/`scp` run with `-o ProxyCommand='<this exe>' ssh-proxy --port N`.
3. The ProxyCommand is a fresh gcs process without the parent's flags, so the
   parent passes its access token in `$GCS_ACCESS_TOKEN` (and the quota project
   in `$GCS_QUOTA_PROJECT`). The child resolves credentials normally and
   picks those up.
4. `ssh-proxy` gets a tunnel token (`:generateAccessToken`) and pipes
   stdin/stdout over the WebSocket as binary frames.

On Unix, gcs `exec`s ssh/scp (`hand_over`) instead of spawning it, so a
signal or `kill` aimed at a backgrounded `gcs ssh -N -L ...` reaches ssh,
which then closes its ProxyCommand. Spawning left ssh orphaned with the
tunnel still up. The exception is `ssh --authorize-session` with refreshable
credentials: gcs stays alive as the parent to re-push credentials every 28
minutes.

Port 2222 is the default: verified live, it accepts ssh's "none" auth over the
tunnel while port 22 offered only `publickey`. gcloud tries 22, then 2222, then
a key pushed with the deprecated `AddPublicKey`. gcs does not retry silently:
an auth failure and a dropped session both exit 255, so a retry could
reconnect a finished session.

User ssh options go before gcs's `-o` options because ssh keeps the first
value it sees, so users can override any gcs default. `ssh::split_options`
mirrors getopt (value-taking letters come from OpenSSH's optstrings) to find
where options end and the remote command starts.

### stdout discipline

In `ssh-proxy` stdout carries SSH bytes, so every message goes to stderr. Keep
progress output (`Starting Cloud Shell...`) on stderr everywhere.

### Process exit

`main` calls `std::process::exit` instead of returning: tokio's stdin reader
sits on a blocking thread that dropping the runtime would wait on forever after
the tunnel closes.

### Networking

HTTP clients use `.no_proxy()`. The WebSocket tunnel has no proxy support, and
proxying only the API calls would fail halfway through `gcs ssh`. TLS is rustls
(aws-lc-rs) with the OS trust store on both paths.

## Build & Test

```bash
cargo build --release   # the binary is named gcs
cargo install --path .  # or: make install
cargo test              # unit tests (no network)
cargo clippy --all-targets
```

### Releasing (macOS arm64)

1. `make bump-patch` (or `bump-minor` / `bump-major`): updates `Cargo.toml`.
2. `make release update-formula`: builds and zips `gcs` as
   `gcs-macos-arm64.zip`, then points `Formula/gcs.rb` at that version's
   download URL with the zip's SHA256.
3. Upload that exact zip to a GitHub release tagged with the bare version
   (`0.1.0`, no `v`). A rebuilt zip has a different hash, so rerun step 2
   after any rebuild.
4. Copy `Formula/gcs.rb` into the `homebrew-tap` repository.
5. `cargo publish --dry-run`, then `cargo publish`, from the committed and
   pushed tree (cargo refuses a dirty one). A published crate version is
   immutable: its metadata, README and files can't be edited, only superseded
   by a new version, so read `cargo package --list` first.

Unit tests cover precedence, ADC parsing and layout, OAuth redirect handling,
argument splitting and API error mapping. The Cloud Shell, tunnel and sign-in
paths were verified by hand against a real account. There is no automated
end-to-end test because it needs a Google sign-in.

## Coding Conventions

- Comments explain **why**, not what
- Pure logic (precedence, argument splitting, error mapping) is kept out of I/O
  so it can be unit-tested
- Tests live in `#[cfg(test)] mod tests` within the same file
- Errors use `anyhow::Result` with messages that say what to do next
- Mirror gcloud's behaviour where one exists, and note where gcs differs
