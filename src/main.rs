mod api;
mod auth;
mod creds;
mod oauth;
mod ssh;
mod tunnel;

use std::process::ExitStatus;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::api::CloudShell;
use crate::creds::{CredFlags, Credentials, QUOTA_ENV, TOKEN_ENV};

const CREDENTIALS_HELP: &str = "\
Credentials, first match wins:
  1. --access-token, --access-token-file or --credentials (only one)
  2. $GCS_ACCESS_TOKEN, then $GCS_ACCESS_TOKEN_FILE, then $GOOGLE_APPLICATION_CREDENTIALS
  3. gcloud's default ADC file (written by `gcs auth login` or
     `gcloud auth application-default login`)";

#[derive(Parser)]
#[command(
    name = "gcs",
    version,
    about = "Google Cloud Shell from your terminal, without the gcloud SDK",
    after_help = CREDENTIALS_HELP
)]
struct Cli {
    #[command(flatten)]
    creds: CredFlags,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in, sign out and inspect credentials
    #[command(subcommand)]
    Auth(AuthCommand),

    /// Show the Cloud Shell environment
    Info {
        /// Print the API's JSON
        #[arg(long)]
        json: bool,
    },

    /// Start Cloud Shell and wait until it is running
    Start,

    /// Connect with ssh(1); arguments are passed to ssh, e.g. `gcs ssh -L 8080:localhost:8080` or `gcs ssh uptime`
    Ssh {
        /// Also sign gcloud in inside Cloud Shell, refreshed while connected
        #[arg(long)]
        authorize_session: bool,

        /// sshd port inside Cloud Shell
        #[arg(long, default_value_t = ssh::DEFAULT_PORT)]
        port: u16,

        /// ssh options, then an optional remote command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "SSH_ARGS")]
        args: Vec<String>,
    },

    /// Copy files with scp(1); mark Cloud Shell paths with cs:, e.g. `gcs scp -r ./dir cs:~/`
    Scp {
        /// sshd port inside Cloud Shell
        #[arg(long, default_value_t = ssh::DEFAULT_PORT)]
        port: u16,

        /// scp options, then sources and destination
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true, value_name = "SCP_ARGS")]
        args: Vec<String>,
    },

    /// Pipe stdin/stdout to Cloud Shell's sshd, for ssh's ProxyCommand
    SshProxy {
        /// Port inside Cloud Shell
        #[arg(long, default_value_t = ssh::DEFAULT_PORT)]
        port: u16,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Sign in with a browser and write an ADC file (like `gcloud auth application-default login`)
    Login {
        /// Paste a code from the browser instead of using a local redirect (for remote machines)
        #[arg(long)]
        no_browser: bool,
    },
    /// Revoke the ADC file's credentials and delete it
    Logout,
    /// Show which credentials are in use and whether they work
    Status,
    /// Print an access token
    Token,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    let code = match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    // Exit here rather than returning: ssh-proxy's stdin reader sits on a
    // blocking thread that dropping the runtime would wait on forever.
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<i32> {
    let flags = &cli.creds;
    flags.ensure_single()?;
    let http = http_client()?;
    match cli.command {
        Command::Auth(AuthCommand::Login { no_browser }) => auth::login(&http, flags, no_browser).await.map(|_| 0),
        Command::Auth(AuthCommand::Logout) => auth::logout(&http, flags).await.map(|_| 0),
        Command::Auth(AuthCommand::Status) => auth::status(&http, flags).await,
        Command::Auth(AuthCommand::Token) => auth::token(&http, flags).await.map(|_| 0),
        Command::Info { json } => info(&http, flags, json).await.map(|_| 0),
        Command::Start => {
            let (_, _, api) = connect(&http, flags).await?;
            api.ensure_running(None).await?;
            println!("Cloud Shell is running.");
            Ok(0)
        }
        Command::Ssh { authorize_session, port, args } => ssh(http, flags, authorize_session, port, &args).await,
        Command::Scp { port, args } => scp(&http, flags, port, &args).await,
        Command::SshProxy { port } => {
            let (_, _, api) = connect(&http, flags).await?;
            let env = api.ensure_running(None).await?;
            let tunnel_token = api.tunnel_token().await?;
            tunnel::pipe_stdio(&env.web_host, &tunnel_token, port).await?;
            Ok(0)
        }
    }
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // Direct connections only: the WebSocket tunnel has no proxy support,
        // and proxying just the API calls would fail halfway through `gcs ssh`.
        .no_proxy()
        .user_agent(concat!("gcs/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .context("creating the HTTP client")
}

fn cloud_shell(http: &reqwest::Client, creds: &Credentials, token: String) -> CloudShell {
    CloudShell::new(http.clone(), token, creds.quota.clone(), creds.rejected_hint())
}

/// ssh or scp with gcs as the ProxyCommand. That ProxyCommand is a fresh gcs
/// process without our flags, so hand it the token we already hold (and the
/// quota project) to act as the same identity.
fn openssh(program: &str, argv: &[String], creds: &Credentials, token: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.args(argv).env(TOKEN_ENV, token);
    match creds.quota_project() {
        Some(project) => cmd.env(QUOTA_ENV, project),
        None => cmd.env_remove(QUOTA_ENV),
    };
    cmd
}

/// Becomes ssh/scp instead of running it as a child, so a signal or `kill`
/// aimed at gcs (say, a backgrounded `gcs ssh -N -L ...`) reaches ssh rather
/// than orphaning it. Only returns if the exec fails.
#[cfg(unix)]
fn hand_over(mut cmd: std::process::Command, program: &str) -> Result<i32> {
    let err = std::os::unix::process::CommandExt::exec(&mut cmd);
    Err(err).with_context(|| format!("running {program}; is OpenSSH installed?"))
}

#[cfg(not(unix))]
fn hand_over(mut cmd: std::process::Command, program: &str) -> Result<i32> {
    let status = cmd.status().with_context(|| format!("running {program}; is OpenSSH installed?"))?;
    Ok(exit_code(status))
}

async fn connect(http: &reqwest::Client, flags: &CredFlags) -> Result<(Credentials, String, CloudShell)> {
    let creds = Credentials::resolve(flags)?;
    let token = creds.access_token(http).await?;
    let api = cloud_shell(http, &creds, token.clone());
    Ok((creds, token, api))
}

async fn info(http: &reqwest::Client, flags: &CredFlags, json: bool) -> Result<()> {
    let (_, _, api) = connect(http, flags).await?;
    let env = api.environment_json().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&env)?);
        return Ok(());
    }
    let field = |name: &str| match env.get(name) {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "-".to_string(),
    };
    for (label, name) in [("state", "state"), ("user", "sshUsername"), ("web host", "webHost"), ("image", "dockerImage")] {
        println!("{label:<9} {}", field(name));
    }
    Ok(())
}

// gcloud refreshes Cloud Shell's credentials on the same cadence: often
// enough that the one-hour token pushed into Cloud Shell never lapses.
const REAUTHORIZE_EVERY: Duration = Duration::from_secs(28 * 60);

async fn ssh(http: reqwest::Client, flags: &CredFlags, authorize: bool, port: u16, args: &[String]) -> Result<i32> {
    let (creds, token, api) = connect(&http, flags).await?;
    let env = api.ensure_running(authorize.then_some(token.as_str())).await?;
    if authorize {
        api.authorize(&token).await?;
    }
    let argv = ssh::ssh_args(&env.ssh_username, args, &self_proxy_command(port)?);
    let cmd = openssh("ssh", &argv, &creds, &token);
    if !(authorize && creds.refreshable()) {
        return hand_over(cmd, "ssh");
    }

    // --authorize-session keeps gcs alive as ssh's parent to refresh the
    // credentials it pushed into Cloud Shell while the session lasts.
    let mut child = tokio::process::Command::from(cmd)
        .spawn()
        .context("running ssh; is OpenSSH installed?")?;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(REAUTHORIZE_EVERY).await;
            let refreshed = async {
                let token = creds.access_token(&http).await?;
                cloud_shell(&http, &creds, token.clone()).authorize(&token).await
            };
            if let Err(e) = refreshed.await {
                eprintln!("warning: could not refresh gcloud's credentials in Cloud Shell: {e:#}");
            }
        }
    });
    Ok(exit_code(child.wait().await?))
}

async fn scp(http: &reqwest::Client, flags: &CredFlags, port: u16, args: &[String]) -> Result<i32> {
    ssh::check_scp_args(args)?;
    let (creds, token, api) = connect(http, flags).await?;
    let env = api.ensure_running(None).await?;
    let argv = ssh::scp_args(&env.ssh_username, args, &self_proxy_command(port)?)?;
    hand_over(openssh("scp", &argv, &creds, &token), "scp")
}

fn self_proxy_command(port: u16) -> Result<String> {
    let exe = std::env::current_exe().context("locating the gcs executable")?;
    Ok(ssh::proxy_command(&exe, port))
}

fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("gcs").chain(args.iter().copied()))
    }

    #[test]
    fn ssh_passes_hyphenated_args_through() {
        let cli = parse(&["ssh", "--authorize-session", "-L", "8080:localhost:80", "uptime"]).unwrap();
        match cli.command {
            Command::Ssh { authorize_session, port, args } => {
                assert!(authorize_session);
                assert_eq!(port, ssh::DEFAULT_PORT);
                assert_eq!(args, ["-L", "8080:localhost:80", "uptime"]);
            }
            _ => panic!("expected ssh"),
        }
    }

    #[test]
    fn ssh_accepts_double_dash() {
        let cli = parse(&["ssh", "--", "-v"]).unwrap();
        assert!(matches!(cli.command, Command::Ssh { args, .. } if args == ["-v"]));
    }

    #[test]
    fn global_credential_flag_after_subcommand() {
        let cli = parse(&["info", "--credentials", "/x.json"]).unwrap();
        assert_eq!(cli.creds.credentials.as_deref(), Some(std::path::Path::new("/x.json")));
    }

    #[test]
    fn two_credential_flags_conflict_wherever_they_are_given() {
        for args in [
            &["--access-token", "t", "--credentials", "/x.json", "info"][..],
            &["info", "--access-token-file", "/t", "--access-token", "t"][..],
        ] {
            let err = parse(args).unwrap().creds.ensure_single().unwrap_err();
            assert!(err.to_string().contains("can't be used together"), "{err}");
        }
        assert!(parse(&["info", "--credentials", "/x.json"]).unwrap().creds.ensure_single().is_ok());
    }

    #[test]
    fn scp_requires_arguments() {
        assert!(parse(&["scp"]).is_err());
    }
}
