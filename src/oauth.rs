//! Google OAuth for the ADC file gcs writes, mirroring
//! `gcloud auth login --update-adc` and `gcloud auth application-default revoke`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::creds::AuthorizedUser;

// gcloud CLI's own OAuth client, the one `gcloud auth login --update-adc`
// puts in ADC. Not gcloud's separate ADC client: the Cloud Shell API refuses
// that one unless a quota project is set, while this one needs no project,
// as with `gcloud cloud-shell ssh`. It is an "installed app" client, so the
// secret is not confidential; gcloud ships it in plain text too.
pub const CLIENT_ID: &str = "32555940559.apps.googleusercontent.com";
pub const CLIENT_SECRET: &str = "ZmssLNjJy2998hD4CTg2ejr2";

const AUTH_URI: &str = "https://accounts.google.com/o/oauth2/auth";
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URI: &str = "https://oauth2.googleapis.com/revoke";
const TOKENINFO_URI: &str = "https://oauth2.googleapis.com/tokeninfo";
// Google shows the authorization code on this page for gcloud's CLI client,
// for sign-ins where no local browser can reach a loopback server.
const CODE_PAGE_REDIRECT: &str = "https://sdk.cloud.google.com/authcode.html";

pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
// The scopes `gcloud auth login` asks for, minus accounts.reauth, which only
// serves gcloud's own re-authentication prompt.
const SCOPES: [&str; 6] = [
    "openid",
    "https://www.googleapis.com/auth/userinfo.email",
    CLOUD_PLATFORM_SCOPE,
    "https://www.googleapis.com/auth/appengine.admin",
    "https://www.googleapis.com/auth/sqlservice.login",
    "https://www.googleapis.com/auth/compute",
];

// gcloud searches the same range for its loopback server.
const LOOPBACK_PORTS: std::ops::Range<u16> = 8085..8185;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    scope: Option<String>,
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct OAuthError {
    error: String,
    error_description: Option<String>,
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.error_description {
            Some(d) if !d.is_empty() => write!(f, "{}: {d}", self.error),
            _ => write!(f, "{}", self.error),
        }
    }
}

async fn post_token(http: &reqwest::Client, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let resp = http.post(TOKEN_URI).form(form).send().await.context("contacting Google's token endpoint")?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        if let Ok(err) = serde_json::from_str::<OAuthError>(&body) {
            // Revoked or expired sign-ins, and accounts whose session policy
            // demands re-authentication (invalid_rapt), all land here.
            if err.error == "invalid_grant" {
                bail!("Google no longer accepts these credentials ({err}); sign in again with `gcs auth login`");
            }
            bail!("Google refused the token request ({err})");
        }
        bail!("Google's token endpoint returned {status}: {body}");
    }
    serde_json::from_str(&body).context("unexpected token response")
}

pub async fn refresh(http: &reqwest::Client, user: &AuthorizedUser) -> Result<String> {
    let resp = post_token(
        http,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &user.client_id),
            ("client_secret", &user.client_secret),
            ("refresh_token", &user.refresh_token),
        ],
    )
    .await?;
    Ok(resp.access_token)
}

pub enum Revoked {
    Yes,
    /// Google no longer recognises the token, e.g. it was already revoked.
    NotRevocable,
}

pub async fn revoke(http: &reqwest::Client, token: &str) -> Result<Revoked> {
    let resp = http
        .post(REVOKE_URI)
        .form(&[("token", token)])
        .send()
        .await
        .context("contacting Google's revoke endpoint")?;
    if resp.status().is_success() {
        return Ok(Revoked::Yes);
    }
    let status = resp.status();
    let body = resp.text().await?;
    match serde_json::from_str::<OAuthError>(&body) {
        Ok(err) if err.error == "invalid_token" => Ok(Revoked::NotRevocable),
        Ok(err) => bail!("Google refused to revoke the token ({err})"),
        Err(_) => bail!("Google's revoke endpoint returned {status}: {body}"),
    }
}

#[derive(Debug, Deserialize)]
pub struct TokenInfo {
    pub email: Option<String>,
    #[serde(default)]
    pub scope: String,
    pub expires_in: Option<String>,
}

/// Ok(None) when Google says the token is invalid or expired.
pub async fn tokeninfo(http: &reqwest::Client, token: &str) -> Result<Option<TokenInfo>> {
    let resp = http
        .get(TOKENINFO_URI)
        .query(&[("access_token", token)])
        .send()
        .await
        .context("contacting Google's tokeninfo endpoint")?;
    if resp.status() == reqwest::StatusCode::BAD_REQUEST {
        return Ok(None);
    }
    let resp = resp.error_for_status().context("Google's tokeninfo endpoint")?;
    Ok(Some(resp.json().await.context("unexpected tokeninfo response")?))
}

pub struct Login {
    pub refresh_token: String,
    pub email: Option<String>,
}

/// Runs the sign-in. With `paste_code`, Google shows a code to paste back
/// instead of redirecting to a loopback server, so it works on machines whose
/// browser runs elsewhere.
pub async fn login(http: &reqwest::Client, paste_code: bool) -> Result<Login> {
    let verifier = random_urlsafe(64)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(24)?;

    let (code, redirect_uri) = if paste_code {
        let url = auth_url(CODE_PAGE_REDIRECT, &state, &challenge, true);
        eprintln!("Open this link in a browser and complete the sign-in:\n\n    {url}\n");
        (prompt_for_code()?, CODE_PAGE_REDIRECT.to_string())
    } else {
        let (listener, port) = bind_loopback().await?;
        let redirect_uri = format!("http://localhost:{port}/");
        let url = auth_url(&redirect_uri, &state, &challenge, false);
        if open_browser(&url) {
            eprintln!("Your browser has been opened to visit:\n\n    {url}\n");
        } else {
            eprintln!("Open this link in a browser on this machine:\n\n    {url}\n");
        }
        let code = tokio::time::timeout(LOGIN_TIMEOUT, receive_code(listener, &state))
            .await
            .context("timed out after 5 minutes waiting for the browser sign-in")??;
        (code, redirect_uri)
    };

    let resp = post_token(
        http,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("code_verifier", &verifier),
        ],
    )
    .await?;

    if let Some(scope) = &resp.scope {
        if !scope.split(' ').any(|s| s == CLOUD_PLATFORM_SCOPE) {
            bail!(
                "{CLOUD_PLATFORM_SCOPE} is required but was not granted; \
                 run `gcs auth login` again and allow it on the consent page"
            );
        }
    }
    let refresh_token = resp.refresh_token.context("Google returned no refresh token")?;
    let email = resp.id_token.as_deref().and_then(id_token_email);
    Ok(Login { refresh_token, email })
}

fn auth_url(redirect_uri: &str, state: &str, challenge: &str, paste_code: bool) -> reqwest::Url {
    let mut url = reqwest::Url::parse(AUTH_URI).expect("valid constant URL");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &SCOPES.join(" "))
            .append_pair("state", state)
            .append_pair("access_type", "offline")
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        if paste_code {
            // As gcloud does: the token is used on this machine, not on the
            // device whose browser gives consent.
            q.append_pair("prompt", "consent").append_pair("token_usage", "remote");
        }
    }
    url
}

fn random_urlsafe(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("no system randomness: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

fn prompt_for_code() -> Result<String> {
    eprint!("Enter the authorization code shown in the browser: ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).context("reading the authorization code")?;
    let code = line.trim();
    if code.is_empty() {
        bail!("no authorization code entered");
    }
    Ok(code.to_string())
}

async fn bind_loopback() -> Result<(TcpListener, u16)> {
    for port in LOOPBACK_PORTS {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok((listener, port));
        }
    }
    bail!(
        "no free local port between {} and {} for the sign-in redirect; \
         free one, or use `gcs auth login --no-browser`",
        LOOPBACK_PORTS.start,
        LOOPBACK_PORTS.end - 1
    )
}

/// Serves the loopback redirect until a request carries the authorization
/// result. Other requests (a browser's favicon fetch) get a 404.
async fn receive_code(listener: TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let target = match read_request_target(&mut sock).await {
            Ok(target) => target,
            Err(_) => continue,
        };
        match parse_redirect(&target, state) {
            Redirect::Code(code) => {
                respond(&mut sock, "200 OK", "Signed in. You can close this tab and return to the terminal.").await;
                return Ok(code);
            }
            Redirect::Failed(reason) => {
                respond(&mut sock, "400 Bad Request", &format!("Sign-in failed: {reason}")).await;
                bail!("sign-in failed: {reason}");
            }
            Redirect::Other => respond(&mut sock, "404 Not Found", "Not found.").await,
        }
    }
}

async fn read_request_target(sock: &mut tokio::net::TcpStream) -> Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(2).any(|w| w == b"\r\n") {
        let n = sock.read(&mut chunk).await?;
        if n == 0 || buf.len() > 64 * 1024 {
            bail!("incomplete request");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().next().unwrap_or_default();
    Ok(line.split_whitespace().nth(1).unwrap_or_default().to_string())
}

async fn respond(sock: &mut tokio::net::TcpStream, status: &str, message: &str) {
    // Browsers usually ignore window.close() on a tab the user opened, so
    // the message stays as the fallback.
    let body = format!("<!doctype html><title>gcs</title><p>{message}</p><script>window.close()</script>");
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    let _ = sock.shutdown().await;
}

#[derive(Debug, PartialEq)]
enum Redirect {
    Code(String),
    Failed(String),
    Other,
}

fn parse_redirect(target: &str, state: &str) -> Redirect {
    let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else {
        return Redirect::Other;
    };
    if url.path() != "/" {
        return Redirect::Other;
    }
    let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
    let (code, error) = (param("code"), param("error"));
    if code.is_none() && error.is_none() {
        return Redirect::Other;
    }
    if param("state").as_deref() != Some(state) {
        return Redirect::Failed("the response did not match this sign-in (state mismatch)".into());
    }
    match (code, error) {
        (_, Some(error)) => Redirect::Failed(error),
        (Some(code), None) => Redirect::Code(code),
        (None, None) => Redirect::Other,
    }
}

/// The email claim of an ID token, for display only: the token came straight
/// from Google's token endpoint over TLS, so it is not verified again here.
fn id_token_email(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let json = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&json).ok()?;
    claims.get("email")?.as_str().map(str::to_string)
}

/// Whether to open a local browser for sign-in. Linux with no display gets the
/// paste-code flow automatically, as gcloud does.
pub fn can_open_browser() -> bool {
    if cfg!(target_os = "linux") {
        return std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    }
    true
}

fn open_browser(url: &reqwest::Url) -> bool {
    use std::process::{Command, Stdio};
    let url = url.as_str();
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_with_code_and_matching_state() {
        assert_eq!(parse_redirect("/?state=s1&code=4%2Fabc&scope=x", "s1"), Redirect::Code("4/abc".into()));
    }

    #[test]
    fn redirect_with_wrong_state_fails() {
        assert!(matches!(parse_redirect("/?state=evil&code=c", "s1"), Redirect::Failed(_)));
    }

    #[test]
    fn redirect_with_error_fails() {
        assert_eq!(parse_redirect("/?state=s1&error=access_denied", "s1"), Redirect::Failed("access_denied".into()));
    }

    #[test]
    fn unrelated_requests_are_ignored() {
        assert_eq!(parse_redirect("/favicon.ico", "s1"), Redirect::Other);
        assert_eq!(parse_redirect("/", "s1"), Redirect::Other);
    }

    #[test]
    fn auth_url_carries_pkce_and_offline_access() {
        let url = auth_url("http://localhost:8085/", "st", "ch", false);
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], CLIENT_ID);
        assert_eq!(q["redirect_uri"], "http://localhost:8085/");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["access_type"], "offline");
        assert!(q["scope"].contains(CLOUD_PLATFORM_SCOPE));
        assert!(!q.contains_key("prompt"));
        let pasted = auth_url(CODE_PAGE_REDIRECT, "st", "ch", true);
        assert!(pasted.as_str().contains("token_usage=remote"));
    }

    #[test]
    fn reads_email_from_id_token() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"email":"someone@example.com"}"#);
        assert_eq!(id_token_email(&format!("h.{payload}.sig")).as_deref(), Some("someone@example.com"));
        assert_eq!(id_token_email("garbage"), None);
    }
}
