---
name: gcs
description: Use when the user asks to run a command in Google Cloud Shell, copy files to or from Cloud Shell, open a shell or a port forward into Cloud Shell, connect an editor to Cloud Shell over SSH, or check or renew the Google sign-in gcs uses (gcloud's ADC file), all without the gcloud SDK.
user-invocable: true
---

# gcs — Google Cloud Shell CLI

`gcs` reaches the user's Google Cloud Shell from this machine without the
gcloud SDK. It runs commands over SSH, copies files with scp and forwards
ports. It reads gcloud's Application Default Credentials (ADC) file, so a
sign-in from either gcs or gcloud on this machine is enough.

## Before the first command: check the sign-in

```bash
gcs auth status
```

- **Exit 0**: ready. It prints where the credentials come from, the account
  and when the current token expires.
- **Anything else**: not usable. Stderr, or the `token` / `scope` line, says
  why; see [Failure handling](#failure-handling). Don't go on to other commands.

`auth status` only reads; it never changes credentials.

## Signing in needs the human

`gcs auth login` opens a browser on this machine and waits up to 5 minutes for
the user to finish Google's sign-in. You cannot complete it for them.

- **Ask before running it.** It overwrites gcloud's default ADC file, which
  other tools on the machine also read (gcloud, Google client libraries,
  Terraform). If the user agrees, run it in the background, tell them to finish
  the sign-in in the browser tab that opened, and wait for it to exit.
- **Don't run `--no-browser` yourself.** It reads a code from stdin that only
  the user can see. On a machine without a browser, ask the user to run
  `gcs auth login --no-browser` in their own terminal.
- **Never run `gcs auth logout` unless the user asks.** It revokes the sign-in
  for every tool that shares the ADC file.

## Standard patterns

### Run a command

```bash
gcs ssh 'uname -a'
gcs ssh 'cd ~/app && git pull && make test'
```

- **Always pass a command.** A bare `gcs ssh` opens an interactive shell,
  which needs a human at a terminal.
- **Single-quote it.** Your local shell expands `~`, `$VAR` and globs in
  unquoted text before gcs sees them.
- **The exit code is the remote command's.** 255 means ssh itself failed
  (connection or sign-in), not the command.
- **The first command may start Cloud Shell.** A suspended Cloud Shell prints
  `Starting Cloud Shell...` on stderr and takes around half a minute to come
  up. That is not a hang: give the first call a timeout of a few minutes.
- Add `-tt` only when a program refuses to run without a terminal.

### Copy files

```bash
gcs scp ./build.tar.gz cs:~/            # upload
gcs scp cs:~/out/report.csv .           # download
gcs scp -r ./site cs:~/site             # a directory
```

`cs:` marks a Cloud Shell path, and a relative path after it is relative to
the Cloud Shell home directory. At least one path must be a `cs:` path.
Options such as `-r` go before the paths.

### Forward a port

```bash
gcs ssh -N -L 8080:localhost:8080       # forward only; blocks while it's up
gcs ssh -f -N -L 8080:localhost:8080    # same, but returns once connected
```

- Without `-N`, ssh also opens a shell, and the forward lasts only while that
  shell is open.
- The target `localhost:8080` is resolved inside Cloud Shell, so something
  there must be listening on that port. `-L` reaches that one address only.
- To send arbitrary traffic out through Cloud Shell (for example, to reach
  sites the local network blocks), use `-D` for a SOCKS5 proxy:
  `gcs ssh -f -N -D 7890`, then
  `curl -x socks5h://127.0.0.1:7890 https://www.google.com`. It is SOCKS only;
  using it as an HTTP proxy fails with `Proxy CONNECT aborted`.
- To stop a forward, kill its process. On macOS and Linux the gcs process
  becomes `ssh`, so killing it closes the tunnel. A forward started with `-f`
  has no job to kill; find it by its local port:
  `kill $(lsof -t -iTCP:8080 -sTCP:LISTEN)`.

### Long-running work

Only `$HOME` survives a Cloud Shell restart: packages installed with `apt` and
files elsewhere are gone afterwards. Cloud Shell also shuts down when it has
been idle for a while, which may end detached jobs too. Detach long jobs on
the remote side and check on them:

```bash
gcs ssh 'nohup ./long-job.sh > ~/long-job.log 2>&1 < /dev/null &'
gcs ssh 'cd ~/app && { nohup ./server > ~/server.log 2>&1 < /dev/null & }'
gcs ssh 'tail -n 20 ~/long-job.log'
```

Two remote-shell traps, both seen in practice:

- **Background only the detached command.** In `cd ~/app && nohup ./server
  ... &` the `&` backgrounds the whole `&&` chain, which keeps the
  connection's output open, so `gcs ssh` never returns. Wrap the detached part
  in `{ ...; }` as above.
- **`pkill -f` can kill its own shell.** The remote shell's command line is
  your whole command string, so `pkill -f PATTERN` matches it whenever that
  string contains PATTERN. The shell dies with exit 255 and no output. Use a
  bracket pattern (`pkill -f '[h]ttp.server 8080'`) and run it in its own
  `gcs ssh` call.

### gcloud inside Cloud Shell

Remote commands that call gcloud need `--authorize-session`, so that gcloud in
Cloud Shell acts as the signed-in user:

```bash
gcs ssh --authorize-session 'gcloud projects list'
```

With this flag gcs stays running as ssh's parent and refreshes those
credentials every 28 minutes, so stop it with its whole process group, not
just its PID.

### State for scripts

```bash
gcs info --json | jq -r .state          # RUNNING, SUSPENDED, PENDING, ...
gcs info --json | jq -r .sshUsername
gcs start                               # start and wait until running
```

## Command reference

| Command | Purpose |
|---|---|
| `gcs auth status` | Which credentials are in use and whether they work. Read-only. |
| `gcs auth login [--no-browser]` | Browser sign-in that writes the ADC file. Needs the user. |
| `gcs auth logout` | Revoke the ADC file's sign-in and delete the file. Only on request. |
| `gcs auth token` | Print an access token. Never echo it into the conversation or logs; use it inline, e.g. `GCS_ACCESS_TOKEN=$(gcs auth token) ...`. |
| `gcs info [--json]` | Cloud Shell state, username, host and image. Doesn't start it. |
| `gcs start` | Start Cloud Shell and wait until it runs. |
| `gcs ssh [--authorize-session] [--port N] [SSH_ARGS...]` | ssh into Cloud Shell; ssh options first, then the remote command. |
| `gcs scp [--port N] [SCP_ARGS...]` | scp with `cs:` paths. |
| `gcs ssh-proxy [--port N]` | stdin/stdout tunnel to Cloud Shell's sshd, for an ssh `ProxyCommand`. |

gcs options (`--credentials`, `--port`, `--authorize-session`, ...) must come
**before** the ssh/scp arguments: everything after the first ssh argument goes
to ssh. `gcs ssh 'cmd' --authorize-session` runs `cmd --authorize-session`
remotely.

## Credentials

First match wins:

1. One flag: `--access-token TOKEN`, `--access-token-file PATH` or
   `--credentials PATH` (an ADC file). Two at once is an error.
2. `$GCS_ACCESS_TOKEN`, then `$GCS_ACCESS_TOKEN_FILE`, then
   `$GOOGLE_APPLICATION_CREDENTIALS`.
3. gcloud's default ADC file (`~/.config/gcloud/application_default_credentials.json`,
   or under `$CLOUDSDK_CONFIG`).

- Access tokens are used as given and never refreshed; ADC files are refreshed
  automatically.
- `--access-token` is visible in `ps` and shell history. Prefer the variable
  or a token file.
- Cloud Shell belongs to a user, so service-account keys don't work.
- `--quota-project PROJECT` / `$GCS_QUOTA_PROJECT` bills API calls to a
  project. Only credentials from `gcloud auth application-default login` need
  it; a `gcs auth login` sign-in doesn't.

## Failure handling

Retry a network error at most once. Never loop on sign-in errors: they need
the user.

| Error says | Meaning | Do |
|---|---|---|
| `no credentials found` | No sign-in on this machine. | Ask the user to sign in (see above). |
| `Google no longer accepts these credentials (invalid_grant ...)` or `Google rejected the credentials ...` | The sign-in was revoked or expired. `invalid_rapt` means the account's policy demands periodic re-authentication. | Ask the user to run `gcs auth login`. |
| `the access token ... is invalid or expired` | A token from a flag or `GCS_ACCESS_TOKEN*`, which gcs never refreshes. | Get a fresh token, or unset the variable so gcs falls back to the ADC file. |
| `missing cloud-platform` (from `auth status`) | The token wasn't issued with the scope Cloud Shell requires. | Use a token with the `cloud-platform` scope, or the ADC file. |
| `` holds `service_account` credentials `` | `$GOOGLE_APPLICATION_CREDENTIALS` points at a service-account key. | Use `--credentials` with a user ADC file, or ask the user to sign in. |
| `these credentials need a quota project` | ADC from gcloud's ADC client. | Ask the user for a project with the Cloud Shell API enabled and pass `--quota-project`, or have them run `gcs auth login`. |
| `your account is unverified` | Google wants the account verified before Cloud Shell starts. | Give the user the printed link; only they can do it. |
| `Cloud Shell did not start ...` | Start failed or took over 5 minutes. | Retry once, then report the error text to the user. |
| ssh exit 255, `Permission denied` | Cloud Shell refused the key-less login on that port (gcs defaults to 2222). | Don't loop. Report it; gcs has no key-based fallback. |
| `running ssh; is OpenSSH installed?` | No `ssh`/`scp` on `PATH`. | Ask the user to install the OpenSSH client. |
| `opening the tunnel ...` / `contacting the Cloud Shell API` | Network trouble. gcs connects directly and ignores `HTTPS_PROXY`. | Retry once, then report. |

## Editors (VS Code Remote-SSH, Zed, ...)

Add this to `~/.ssh/config`, with the username from
`gcs info --json | jq -r .sshUsername`:

```
Host cloudshell
    User YOUR_CLOUD_SHELL_USERNAME
    ProxyCommand gcs ssh-proxy
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
```

`gcs` must be on the `PATH` the editor sees. Cloud Shell gets a new host key
whenever it starts, hence the two host-key lines.
