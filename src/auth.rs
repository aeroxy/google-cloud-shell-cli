//! `gcs auth ...`: the ADC file is shared with gcloud and Google's client
//! libraries, so it is written exactly as `gcloud auth login --update-adc`
//! writes it.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::creds::{self, ADC_ENV, CredFlags, Credentials, Kind, Source, TOKEN_ENV, TOKEN_FILE_ENV};
use crate::oauth;

/// login and logout act on the file login writes: `--credentials` or the
/// default path, never a file found through `$GOOGLE_APPLICATION_CREDENTIALS`,
/// which may be someone else's key file (gcloud draws the same line).
fn target_file(flags: &CredFlags, command: &str) -> Result<PathBuf> {
    if flags.access_token.is_some() || flags.access_token_file.is_some() {
        bail!("`gcs auth {command}` works on ADC files; use --credentials to pick one instead of an access token");
    }
    match &flags.credentials {
        Some(path) => Ok(path.clone()),
        None => creds::default_adc_path(),
    }
}

pub async fn login(http: &reqwest::Client, flags: &CredFlags, no_browser: bool) -> Result<()> {
    let path = target_file(flags, "login")?;
    let login = oauth::login(http, no_browser || !oauth::can_open_browser()).await?;
    // No quota project, as with `gcloud auth login --update-adc`: the client
    // gcs signs in with needs none, and a stale one could make the Cloud
    // Shell API refuse requests.
    write_private(&path, &adc_json(&login.refresh_token))?;

    match &login.email {
        Some(email) => eprintln!("Signed in as {email}."),
        None => eprintln!("Signed in."),
    }
    eprintln!("Credentials saved to {}", path.display());
    if flags.credentials.is_none() {
        for name in [TOKEN_ENV, TOKEN_FILE_ENV] {
            if env_set(name) {
                eprintln!("note: ${name} is set and takes precedence over this login");
            }
        }
        if std::env::var_os(ADC_ENV).is_some_and(|v| !v.is_empty() && Path::new(&v) != path) {
            eprintln!("note: ${ADC_ENV} is set and takes precedence over this login");
        }
    }
    Ok(())
}

pub async fn logout(http: &reqwest::Client, flags: &CredFlags) -> Result<()> {
    if flags.access_token.is_some() || flags.access_token_file.is_some() {
        bail!(
            "gcs does not revoke access tokens: Google would also revoke the refresh token \
             that issued it, signing out whatever tool created the token"
        );
    }
    let path = target_file(flags, "logout")?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("No credentials at {}; nothing to revoke.", path.display());
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let user = creds::parse_adc(&text).with_context(|| format!("{} was left untouched", path.display()))?;
    let revoked = oauth::revoke(http, &user.refresh_token).await?;
    std::fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))?;
    match revoked {
        oauth::Revoked::Yes => eprintln!("Credentials revoked and {} deleted.", path.display()),
        oauth::Revoked::NotRevocable => eprintln!(
            "Google no longer recognises these credentials (already revoked?); deleted {}.",
            path.display()
        ),
    }
    if flags.credentials.is_none() {
        for name in [TOKEN_ENV, TOKEN_FILE_ENV] {
            if env_set(name) {
                eprintln!("note: ${name} is still set, so gcs keeps using it");
            }
        }
    }
    Ok(())
}

/// Prints where the credentials come from and what Google says about them.
/// Returns exit code 1 when they are unusable for Cloud Shell.
pub async fn status(http: &reqwest::Client, flags: &CredFlags) -> Result<i32> {
    let creds = Credentials::resolve(flags)?;
    let row = |label: &str, value: &str| println!("{label:<9} {value}");

    row("source", &creds.choice.origin.to_string());
    match &creds.choice.source {
        Source::Token(_) => {}
        Source::TokenFile(path) | Source::Adc(path) => row("file", &path.display().to_string()),
    }
    row(
        "type",
        match creds.kind {
            Kind::Token(_) => "access token (used as given, never refreshed)",
            Kind::User(_) => "user credentials (refreshed automatically)",
        },
    );
    if let Some(quota) = &creds.quota {
        row("quota", &format!("{} (from {})", quota.project, quota.origin));
    }

    let token = creds.access_token(http).await?;
    let Some(info) = oauth::tokeninfo(http, &token).await? else {
        row("token", "invalid or expired");
        return Ok(1);
    };
    row("account", info.email.as_deref().unwrap_or("(unknown: no email scope)"));
    if let Some(secs) = info.expires_in.as_deref().and_then(|s| s.parse::<u64>().ok()) {
        row("expires", &format!("in {} min", secs / 60));
    }
    let cloud_platform = info.scope.split(' ').any(|s| s == oauth::CLOUD_PLATFORM_SCOPE);
    row("scope", if cloud_platform { "cloud-platform (ok)" } else { "missing cloud-platform; Cloud Shell will refuse it" });
    Ok(if cloud_platform { 0 } else { 1 })
}

pub async fn token(http: &reqwest::Client, flags: &CredFlags) -> Result<()> {
    let creds = Credentials::resolve(flags)?;
    println!("{}", creds.access_token(http).await?);
    Ok(())
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// Matches gcloud's ADC file: sorted keys, two-space indent, empty `account`.
fn adc_json(refresh_token: &str) -> String {
    let adc = json!({
        "account": "",
        "client_id": oauth::CLIENT_ID,
        "client_secret": oauth::CLIENT_SECRET,
        "refresh_token": refresh_token,
        "type": "authorized_user",
        "universe_domain": "googleapis.com",
    });
    serde_json::to_string_pretty(&adc).expect("JSON values serialize")
}

/// Writes owner-only (0600 on Unix) via a temp file and rename, so a crash
/// never leaves a half-written credentials file.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adc_file_matches_gcloud_layout() {
        let expected = format!(
            r#"{{
  "account": "",
  "client_id": "{}",
  "client_secret": "{}",
  "refresh_token": "1//r",
  "type": "authorized_user",
  "universe_domain": "googleapis.com"
}}"#,
            oauth::CLIENT_ID,
            oauth::CLIENT_SECRET
        );
        assert_eq!(adc_json("1//r"), expected);
    }

    #[test]
    fn written_adc_reads_back_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("adc.json");
        write_private(&path, &adc_json("1//r")).unwrap();
        let user = creds::parse_adc(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(user.refresh_token, "1//r");
        assert_eq!(user.client_id, oauth::CLIENT_ID);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn login_and_logout_refuse_token_flags() {
        let flags = CredFlags { access_token: Some("t".into()), ..Default::default() };
        assert!(target_file(&flags, "login").is_err());
        let flags = CredFlags { credentials: Some("/x/adc.json".into()), ..Default::default() };
        assert_eq!(target_file(&flags, "login").unwrap(), PathBuf::from("/x/adc.json"));
    }
}
