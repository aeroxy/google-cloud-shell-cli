# gcs

Google Cloud Shell from your terminal, without installing the gcloud SDK. One
Rust binary that signs in like gcloud, stores credentials where gcloud does,
and gives you `ssh`, `scp` and an ssh `ProxyCommand` for editors.

```sh
gcs auth login            # browser sign-in, writes gcloud's ADC file
gcs ssh                   # interactive shell
gcs ssh uptime            # run a command
gcs ssh -N -L 8080:localhost:8080   # forward a port, no shell (-f: in the background)
gcs scp -r ./site cs:~/site         # cs: marks Cloud Shell paths
```

## Install

```sh
cargo install --path .    # or: make install
```

Needs the OpenSSH client (`ssh`, `scp`) on your `PATH`; gcs runs them for you.

## Commands

| Command | What it does |
|---|---|
| `gcs auth login [--no-browser]` | Sign in with a browser and write an ADC file. `--no-browser` prints a link and asks for the code Google shows, for machines without a local browser (picked automatically on Linux with no display). |
| `gcs auth logout` | Revoke the ADC file's credentials and delete the file. |
| `gcs auth status` | Show which credentials are in use, the account, expiry and scopes. Exits 1 if they won't work. |
| `gcs auth token` | Print an access token. |
| `gcs info [--json]` | Show the Cloud Shell environment: state, username, host, image. |
| `gcs start` | Start Cloud Shell and wait until it is running. |
| `gcs ssh [--authorize-session] [--port N] [SSH_ARGS...]` | Connect with `ssh`. Arguments go to ssh unchanged: options first, then an optional remote command. `--authorize-session` also signs gcloud in inside Cloud Shell and keeps it signed in while you are connected. |
| `gcs scp [--port N] [SCP_ARGS...]` | Copy with `scp`. Prefix Cloud Shell paths with `cs:`, e.g. `gcs scp cs:~/out.log .` |
| `gcs ssh-proxy [--port N]` | Pipe stdin/stdout to Cloud Shell's sshd, for ssh's `ProxyCommand`. |

Port forwarding is plain ssh: `gcs ssh -L 8080:localhost:8080` opens a shell
and forwards local port 8080 to port 8080 inside Cloud Shell while that shell
is open. Add `-N` to forward without a shell, and `-f` as well to put the
forward in the background once it is connected (stop it with `kill`).

`-L` reaches one fixed address inside Cloud Shell. To send *any* traffic out
through Cloud Shell, use `-D`, which turns the tunnel into a SOCKS5 proxy:

```sh
gcs ssh -N -D 7890                                       # SOCKS5 on 127.0.0.1:7890
curl -x socks5h://127.0.0.1:7890 https://www.google.com  # socks5h: DNS resolved in Cloud Shell
```

It is SOCKS only: tools that accept just an HTTP proxy can't use it.

`start`, `ssh`, `scp` and `ssh-proxy` start Cloud Shell first if it is
suspended, which takes about half a minute. gcs options (`--credentials`,
`--port`, …) go before the ssh/scp arguments.

If your organization makes accounts re-authenticate periodically, the sign-in
expires on that schedule; gcs then says so, and `gcs auth login` renews it.

## Credentials

gcs only uses Application Default Credentials (ADC), or an access token you
give it. First match wins:

1. One flag: `--access-token TOKEN`, `--access-token-file PATH` or `--credentials PATH`
2. `$GCS_ACCESS_TOKEN`, then `$GCS_ACCESS_TOKEN_FILE`, then `$GOOGLE_APPLICATION_CREDENTIALS`
3. gcloud's default ADC file: `~/.config/gcloud/application_default_credentials.json`
   (`%APPDATA%\gcloud\…` on Windows, or under `$CLOUDSDK_CONFIG` if set)

- **Access tokens** are used as given and never refreshed. A token file is
  re-read on every run, so another process can keep it fresh. An expired token
  only matters for starting a connection: an open SSH session outlives it.
  `--access-token` shows up in `ps` and shell history; prefer the variable or a
  file.
- **ADC files** must hold a user sign-in (`authorized_user`). Service-account
  keys are refused, because Cloud Shell belongs to a user.
- **`auth login` / `auth logout`** act on the `--credentials` file if given,
  otherwise the default file, never on a file found through
  `$GOOGLE_APPLICATION_CREDENTIALS`. Logout refuses access tokens: Google would
  also revoke the refresh token that issued the token, signing out whatever
  tool created it.

### Sharing the file with gcloud

`gcs auth login` writes the same file `gcloud auth login --update-adc` writes,
using gcloud CLI's sign-in client, so gcloud, Google's client libraries and
Terraform can all read it.

Credentials from `gcloud auth application-default login` work too, but they use
gcloud's separate ADC client, which the Cloud Shell API only accepts with a
quota project: pass `--quota-project PROJECT` or set `$GCS_QUOTA_PROJECT` to a
project with the Cloud Shell API enabled. A `quota_project_id` in the ADC file
is used automatically. Credentials from `gcs auth login` need none.

## Editors (VS Code, Zed, …)

Add a host to `~/.ssh/config`, using the username from `gcs info`:

```
Host cloudshell
    User YOUR_CLOUD_SHELL_USERNAME
    ProxyCommand gcs ssh-proxy
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
```

Then connect to `cloudshell` with the editor's Remote-SSH feature, or run
`ssh cloudshell`. Cloud Shell gets a new host key whenever it starts, hence
the two host-key lines.

## Agent skill

[`skills/gcs/SKILL.md`](skills/gcs/SKILL.md) teaches coding agents (Claude
Code, opencode, …) to use gcs: which steps need you (sign-in), how to run
remote commands safely, and what to do about each error. Install it by
linking the folder into your agent's skills directory, for example:

```sh
ln -s "$PWD/skills/gcs" ~/.claude/skills/gcs
```

## How it works

gcs calls the Cloud Shell API to start your environment and to get a
short-lived tunnel token, then ssh connects through a WebSocket tunnel to
`wss://<webHost>/_cloudshell/tcp/2222`, the same tunnel current gcloud
uses. On that port Cloud Shell lets the tunnelled session in without an SSH key,
so gcs manages no keys. Port 22 can insist on a public key; `--port 22` is
there if Google moves the key-less login back.

## Limitations

- Direct connections only: `HTTPS_PROXY` and friends are ignored, because the
  WebSocket tunnel has no proxy support yet.
- No key-based fallback (gcloud's deprecated `AddPublicKey` path).
- Built and tested on macOS; Linux should work; Windows is untested.

## Development

```sh
make test      # unit tests
make lint      # clippy
make run ARGS="auth status"
```

See [AGENTS.md](AGENTS.md) for the code layout and design notes.
