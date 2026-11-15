//! Builds ssh(1)/scp(1) command lines that reach Cloud Shell through
//! `gcs ssh-proxy`. Arguments pass through to ssh/scp unchanged, apart from
//! gcs inserting the destination and rewriting `cs:` paths.

use std::path::Path;

use anyhow::{Result, bail};

/// The name ssh sees. The connection itself goes through the ProxyCommand.
const HOST: &str = "cloudshell";

/// Cloud Shell's sshd that lets a tunnelled session in without a key
/// (ssh's "none" method). Port 22 can insist on a public key; gcloud tries
/// 22 and then 2222, and 2222 is the one that took key-less logins in testing.
pub const DEFAULT_PORT: u16 = 2222;
pub const REMOTE_PREFIX: &str = "cs:";

// Options that take a value, from the getopt strings of OpenSSH's ssh and scp.
const SSH_VALUE_OPTIONS: &str = "BbcDEeFIiJLlmOoPpQRSWw";
const SCP_VALUE_OPTIONS: &str = "cDFiJloPSX";

/// Splits arguments into the leading options and the operands after them,
/// the way getopt does: options stop at the first operand or at `--`.
fn split_options<'a>(args: &'a [String], value_options: &str) -> (&'a [String], &'a [String]) {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            return (&args[..i], &args[i + 1..]);
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        // In a bundle like `-vL8080:x:80` the first value-taking letter
        // consumes the rest of the word, or the next word if nothing is left.
        let letters = &arg[1..];
        if let Some(pos) = letters.find(|c| value_options.contains(c)) {
            if pos + 1 == letters.len() {
                i += 1;
            }
        }
        i += 1;
    }
    let i = i.min(args.len());
    (&args[..i], &args[i..])
}

/// Options gcs adds. User options come first because ssh keeps the first
/// value it sees, so the user can still override any of these.
fn connection_options(user: &str, proxy_command: &str) -> Vec<String> {
    let known_hosts = if cfg!(windows) { "NUL" } else { "/dev/null" };
    [
        format!("ProxyCommand={proxy_command}"),
        format!("User={user}"),
        // The tunnel is TLS to Google, and Cloud Shell gets a fresh host key
        // on every start, so recording host keys only produces mismatch warnings.
        "StrictHostKeyChecking=no".to_string(),
        format!("UserKnownHostsFile={known_hosts}"),
        "LogLevel=ERROR".to_string(),
        // Same as gcloud: the tunnel token authenticates the session, so no
        // key is offered and nothing is prompted for.
        "PubkeyAuthentication=no".to_string(),
        "PreferredAuthentications=password".to_string(),
        "BatchMode=yes".to_string(),
    ]
    .into_iter()
    .flat_map(|opt| ["-o".to_string(), opt])
    .collect()
}

/// The ProxyCommand that runs this same binary. ssh hands it to the shell,
/// and expands `%` sequences in it first.
pub fn proxy_command(exe: &Path, port: u16) -> String {
    let exe = exe.to_string_lossy().replace('%', "%%");
    let quoted = if cfg!(windows) {
        format!("\"{exe}\"")
    } else {
        format!("'{}'", exe.replace('\'', r"'\''"))
    };
    format!("{quoted} ssh-proxy --port {port}")
}

/// `ssh [user options] [gcs options] cloudshell [command...]`
pub fn ssh_args(user: &str, args: &[String], proxy_command: &str) -> Vec<String> {
    let (options, command) = split_options(args, SSH_VALUE_OPTIONS);
    let mut out = options.to_vec();
    out.extend(connection_options(user, proxy_command));
    out.push(HOST.to_string());
    out.extend_from_slice(command);
    out
}

/// Run before any API call, so a typo doesn't start Cloud Shell.
pub fn check_scp_args(args: &[String]) -> Result<()> {
    let (_, operands) = split_options(args, SCP_VALUE_OPTIONS);
    if operands.len() < 2 {
        bail!("usage: gcs scp [scp options] SOURCE... DEST (mark Cloud Shell paths with {REMOTE_PREFIX}, e.g. {REMOTE_PREFIX}~/file)");
    }
    if !operands.iter().any(|a| a.starts_with(REMOTE_PREFIX)) {
        bail!("no Cloud Shell path given; mark remote paths with {REMOTE_PREFIX}, e.g. {REMOTE_PREFIX}~/file");
    }
    Ok(())
}

/// `scp [user options] [gcs options] operands...` with `cs:PATH` rewritten
/// to `cloudshell:PATH`.
pub fn scp_args(user: &str, args: &[String], proxy_command: &str) -> Result<Vec<String>> {
    check_scp_args(args)?;
    let (options, operands) = split_options(args, SCP_VALUE_OPTIONS);
    let mut out = options.to_vec();
    out.extend(connection_options(user, proxy_command));
    out.extend(operands.iter().map(|a| match a.strip_prefix(REMOTE_PREFIX) {
        Some(path) => format!("{HOST}:{path}"),
        None => a.clone(),
    }));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn options_stop_at_first_operand() {
        let args = s(&["-v", "-L", "8080:localhost:80", "-tt", "ls", "-la"]);
        let (opts, rest) = split_options(&args, SSH_VALUE_OPTIONS);
        assert_eq!(opts, &args[..4]);
        assert_eq!(rest, &args[4..]);
    }

    #[test]
    fn bundled_value_inline_or_next() {
        let args = s(&["-vL8080:x:80", "-vp", "22", "uptime"]);
        let (opts, rest) = split_options(&args, SSH_VALUE_OPTIONS);
        assert_eq!(opts.len(), 3);
        assert_eq!(rest, &s(&["uptime"])[..]);
    }

    #[test]
    fn double_dash_ends_options_and_is_dropped() {
        let args = s(&["-v", "--", "-weird-command"]);
        let (opts, rest) = split_options(&args, SSH_VALUE_OPTIONS);
        assert_eq!(opts, &s(&["-v"])[..]);
        assert_eq!(rest, &s(&["-weird-command"])[..]);
    }

    #[test]
    fn dangling_value_option_does_not_overrun() {
        let args = s(&["-L"]);
        let (opts, rest) = split_options(&args, SSH_VALUE_OPTIONS);
        assert_eq!((opts.len(), rest.len()), (1, 0));
    }

    #[test]
    fn ssh_puts_user_options_first_and_host_before_command() {
        let out = ssh_args("me", &s(&["-o", "LogLevel=DEBUG", "uname", "-a"]), "PROXY");
        assert_eq!(&out[..2], &s(&["-o", "LogLevel=DEBUG"])[..]);
        let host = out.iter().position(|a| a == HOST).unwrap();
        assert_eq!(&out[host + 1..], &s(&["uname", "-a"])[..]);
        assert!(out.contains(&"ProxyCommand=PROXY".to_string()));
        assert!(out.contains(&"User=me".to_string()));
    }

    #[test]
    fn interactive_ssh_ends_with_host() {
        let out = ssh_args("me", &[], "PROXY");
        assert_eq!(out.last().unwrap(), HOST);
    }

    #[test]
    fn scp_rewrites_remote_paths_only() {
        let out = scp_args("me", &s(&["-r", "./dir", "cs:~/dest"]), "PROXY").unwrap();
        assert_eq!(out[0], "-r");
        assert_eq!(&out[out.len() - 2..], &s(&["./dir", "cloudshell:~/dest"])[..]);
    }

    #[test]
    fn scp_value_option_is_not_mistaken_for_operand() {
        let out = scp_args("me", &s(&["-i", "key", "cs:a", "b"]), "PROXY").unwrap();
        assert_eq!(&out[..2], &s(&["-i", "key"])[..]);
        assert_eq!(&out[out.len() - 2..], &s(&["cloudshell:a", "b"])[..]);
    }

    #[test]
    fn scp_needs_a_remote_path() {
        assert!(scp_args("me", &s(&["a", "b"]), "P").is_err());
        assert!(scp_args("me", &s(&["cs:a"]), "P").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn proxy_command_quotes_path_and_escapes_percent() {
        let cmd = proxy_command(Path::new("/opt/my tools/it's%gcs"), 22);
        assert_eq!(cmd, r"'/opt/my tools/it'\''s%%gcs' ssh-proxy --port 22");
    }
}
