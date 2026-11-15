//! The handful of Cloud Shell API v1 calls gcs needs.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::creds::{QUOTA_ENV, Quota};

const BASE: &str = "https://cloudshell.googleapis.com/v1/";
const ENVIRONMENT: &str = "users/me/environments/default";
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const START_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Environment {
    pub state: String,
    pub ssh_username: String,
    pub web_host: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Operation {
    name: String,
    done: bool,
    error: Option<Status>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Status {
    message: String,
    details: Vec<Value>,
}

pub struct CloudShell {
    http: reqwest::Client,
    token: String,
    quota: Option<Quota>,
    /// Shown when Google answers 401, naming where the token came from.
    rejected_hint: String,
}

impl CloudShell {
    pub fn new(http: reqwest::Client, token: String, quota: Option<Quota>, rejected_hint: String) -> Self {
        Self { http, token, quota, rejected_hint }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.http.request(method, format!("{BASE}{path}")).bearer_auth(&self.token);
        if let Some(quota) = &self.quota {
            req = req.header("x-goog-user-project", &quota.project);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.context("contacting the Cloud Shell API")?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(self.api_error(status, &text));
        }
        serde_json::from_str(&text).context("unexpected Cloud Shell API response")
    }

    fn api_error(&self, status: reqwest::StatusCode, body: &str) -> anyhow::Error {
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return anyhow!("{}", self.rejected_hint);
        }
        let message = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.pointer("/error/message")?.as_str().map(str::to_string))
            .unwrap_or_else(|| body.trim().to_string());
        let err = anyhow!("Cloud Shell API returned {status}: {message}");
        if status != reqwest::StatusCode::FORBIDDEN {
            return err;
        }
        match &self.quota {
            Some(quota) => err.context(format!(
                "requests are billed to quota project `{}` (from {}), which needs the Cloud Shell API enabled",
                quota.project, quota.origin
            )),
            // gcloud's ADC client (`gcloud auth application-default login`)
            // gets this; the client `gcs auth login` uses does not.
            None if message.contains("quota project") => err.context(format!(
                "these credentials need a quota project: pass --quota-project or set ${QUOTA_ENV} \
                 to a project with the Cloud Shell API enabled, or sign in with `gcs auth login`, \
                 which needs none"
            )),
            None => err,
        }
    }

    pub async fn environment_json(&self) -> Result<Value> {
        self.call(Method::GET, ENVIRONMENT, None).await
    }

    pub async fn environment(&self) -> Result<Environment> {
        Ok(serde_json::from_value(self.environment_json().await?)?)
    }

    /// Starts Cloud Shell if needed and waits until it accepts connections.
    /// `session_token`, when given, pre-authenticates gcloud inside Cloud Shell.
    pub async fn ensure_running(&self, session_token: Option<&str>) -> Result<Environment> {
        let env = self.environment().await?;
        if env.state == "RUNNING" && !env.web_host.is_empty() {
            return Ok(env);
        }
        eprintln!("Starting Cloud Shell...");
        let body = match session_token {
            Some(token) => json!({ "accessToken": token }),
            None => json!({}),
        };
        let op: Operation =
            serde_json::from_value(self.call(Method::POST, &format!("{ENVIRONMENT}:start"), Some(body)).await?)?;
        let op = tokio::time::timeout(START_TIMEOUT, self.wait(op))
            .await
            .context("Cloud Shell did not start within 5 minutes")??;
        if let Some(status) = op.error {
            bail!(start_error(&status));
        }
        let env = self.environment().await?;
        if env.web_host.is_empty() {
            bail!("Cloud Shell started but reported no host to connect to (state {})", env.state);
        }
        Ok(env)
    }

    async fn wait(&self, mut op: Operation) -> Result<Operation> {
        while !op.done {
            tokio::time::sleep(POLL_INTERVAL).await;
            op = serde_json::from_value(self.call(Method::GET, &op.name, None).await?)?;
        }
        Ok(op)
    }

    /// Sends OAuth credentials into Cloud Shell so gcloud there is signed in.
    pub async fn authorize(&self, access_token: &str) -> Result<()> {
        let body = json!({ "accessToken": access_token });
        self.call(Method::POST, &format!("{ENVIRONMENT}:authorize"), Some(body)).await?;
        Ok(())
    }

    /// A short-lived token for the WebSocket tunnel to Cloud Shell's sshd.
    pub async fn tunnel_token(&self) -> Result<String> {
        let resp = self.call(Method::GET, &format!("{ENVIRONMENT}:generateAccessToken"), None).await?;
        resp.get("accessToken")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("the Cloud Shell API returned no tunnel token")
    }
}

/// Explains a failed start the way gcloud does, including the verification
/// link Google sends for accounts that must be verified first.
fn start_error(status: &Status) -> String {
    let detail = |type_url: &str| {
        status.details.iter().find(|d| d.get("@type").and_then(Value::as_str) == Some(type_url))
    };
    let unverified = detail("type.googleapis.com/google.rpc.ErrorInfo")
        .and_then(|d| d.get("reason")?.as_str())
        == Some("ACCOUNT_UNVERIFIED");
    if !unverified {
        return format!("Cloud Shell did not start: {}", status.message);
    }
    let link = detail("type.googleapis.com/google.rpc.Help")
        .and_then(|d| d.get("links")?.as_array()?.iter().find_map(|l| l.get("url")?.as_str()));
    match link {
        Some(url) => format!(
            "your account is unverified; verify it at {url} or at https://shell.cloud.google.com"
        ),
        None => "your account is unverified; verify it at https://shell.cloud.google.com".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(details: Value) -> Status {
        serde_json::from_value(json!({ "message": "boom", "details": details })).unwrap()
    }

    #[test]
    fn plain_start_failure() {
        assert_eq!(start_error(&status(json!([]))), "Cloud Shell did not start: boom");
    }

    #[test]
    fn unverified_account_links_to_verification() {
        let s = status(json!([
            { "@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "ACCOUNT_UNVERIFIED" },
            { "@type": "type.googleapis.com/google.rpc.Help", "links": [{ "url": "https://verify.example" }] }
        ]));
        assert!(start_error(&s).contains("https://verify.example"));
    }

    #[test]
    fn environment_parses_api_shape() {
        let env: Environment = serde_json::from_value(json!({
            "name": "users/me/environments/default", "state": "RUNNING",
            "sshUsername": "someone", "webHost": "abc.cloudshell.dev", "sshPort": 6000
        }))
        .unwrap();
        assert_eq!((env.state.as_str(), env.ssh_username.as_str()), ("RUNNING", "someone"));
        assert_eq!(env.web_host, "abc.cloudshell.dev");
    }
}
