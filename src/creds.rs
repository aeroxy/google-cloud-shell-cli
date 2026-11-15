//! Where credentials come from, and turning them into an access token.
//!
//! Precedence, most explicit first: a credential flag (at most one), then
//! `$GCS_ACCESS_TOKEN`, `$GCS_ACCESS_TOKEN_FILE`, `$GOOGLE_APPLICATION_CREDENTIALS`,
//! then gcloud's default ADC file. The `GCS_*` variables outrank
//! `GOOGLE_APPLICATION_CREDENTIALS` because that one is often exported globally
//! for other tools, while a `GCS_*` variable is set for gcs on purpose.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::oauth;

pub const TOKEN_ENV: &str = "GCS_ACCESS_TOKEN";
pub const TOKEN_FILE_ENV: &str = "GCS_ACCESS_TOKEN_FILE";
pub const ADC_ENV: &str = "GOOGLE_APPLICATION_CREDENTIALS";
pub const QUOTA_ENV: &str = "GCS_QUOTA_PROJECT";

#[derive(clap::Args, Debug, Default, Clone)]
pub struct CredFlags {
    /// OAuth access token to use as-is. Visible in `ps` and shell history:
    /// prefer $GCS_ACCESS_TOKEN or --access-token-file
    #[arg(long, global = true, value_name = "TOKEN")]
    pub access_token: Option<String>,

    /// File holding an OAuth access token, re-read on every run
    #[arg(long, global = true, value_name = "PATH")]
    pub access_token_file: Option<PathBuf>,

    /// ADC file to use; `auth login` and `auth logout` act on it instead of the default file
    #[arg(long, global = true, value_name = "PATH")]
    pub credentials: Option<PathBuf>,

    /// Project to bill API usage to (also $GCS_QUOTA_PROJECT); overrides the
    /// ADC file's quota_project_id. Only needed for credentials made by
    /// `gcloud auth application-default login`
    #[arg(long, global = true, value_name = "PROJECT")]
    pub quota_project: Option<String>,
}

impl CredFlags {
    /// Two sources at once is a mistake, not a precedence question. Checked
    /// here because clap's argument groups miss global flags given after the
    /// subcommand.
    pub fn ensure_single(&self) -> Result<()> {
        let given: Vec<&str> = [
            (self.access_token.is_some(), "--access-token"),
            (self.access_token_file.is_some(), "--access-token-file"),
            (self.credentials.is_some(), "--credentials"),
        ]
        .into_iter()
        .filter_map(|(set, name)| set.then_some(name))
        .collect();
        if given.len() > 1 {
            bail!("{} can't be used together; pick one credential source", given.join(" and "));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Flag(&'static str),
    Env(&'static str),
    Default,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Flag(name) => write!(f, "{name}"),
            Origin::Env(name) => write!(f, "${name}"),
            Origin::Default => write!(f, "the default ADC file"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Token(String),
    TokenFile(PathBuf),
    Adc(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub source: Source,
    pub origin: Origin,
}

/// Picks the credential source. Pure: the environment and default path are
/// injected so precedence can be tested without touching the real ones.
pub fn choose(flags: &CredFlags, env: impl Fn(&str) -> Option<String>, default_adc: PathBuf) -> Choice {
    // `VAR= gcs ...` is the usual way to unset a variable for one command.
    let env = |name: &str| env(name).filter(|v| !v.is_empty());
    let choice = |source, origin| Choice { source, origin };

    if let Some(token) = &flags.access_token {
        return choice(Source::Token(token.clone()), Origin::Flag("--access-token"));
    }
    if let Some(path) = &flags.access_token_file {
        return choice(Source::TokenFile(path.clone()), Origin::Flag("--access-token-file"));
    }
    if let Some(path) = &flags.credentials {
        return choice(Source::Adc(path.clone()), Origin::Flag("--credentials"));
    }
    if let Some(token) = env(TOKEN_ENV) {
        return choice(Source::Token(token), Origin::Env(TOKEN_ENV));
    }
    if let Some(path) = env(TOKEN_FILE_ENV) {
        return choice(Source::TokenFile(path.into()), Origin::Env(TOKEN_FILE_ENV));
    }
    if let Some(path) = env(ADC_ENV) {
        return choice(Source::Adc(path.into()), Origin::Env(ADC_ENV));
    }
    choice(Source::Adc(default_adc), Origin::Default)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quota {
    pub project: String,
    /// Where the project came from, for `auth status` and error messages.
    pub origin: String,
}

/// The quota project sent as `x-goog-user-project`: flag, then environment,
/// then the ADC file. Empty values count as unset.
pub fn pick_quota(flag: Option<String>, env: Option<String>, file: Option<String>) -> Option<Quota> {
    let candidates = [
        (flag, "--quota-project".to_string()),
        (env, format!("${QUOTA_ENV}")),
        (file, "the ADC file".to_string()),
    ];
    candidates.into_iter().find_map(|(project, origin)| {
        project.filter(|p| !p.is_empty()).map(|project| Quota { project, origin })
    })
}

/// gcloud's config directory, resolved the way gcloud and google-auth do.
pub fn gcloud_config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLOUDSDK_CONFIG").filter(|v| !v.is_empty()) {
        return Ok(dir.into());
    }
    if cfg!(windows) {
        let appdata = std::env::var_os("APPDATA").context("%APPDATA% is not set")?;
        return Ok(PathBuf::from(appdata).join("gcloud"));
    }
    let home = dirs::home_dir().context("cannot determine the home directory")?;
    Ok(home.join(".config").join("gcloud"))
}

pub fn default_adc_path() -> Result<PathBuf> {
    Ok(gcloud_config_dir()?.join("application_default_credentials.json"))
}

/// The fields of an `authorized_user` ADC file that gcs needs.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizedUser {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    #[serde(default)]
    pub quota_project_id: Option<String>,
}

pub enum Kind {
    Token(String),
    User(AuthorizedUser),
}

pub struct Credentials {
    pub choice: Choice,
    pub kind: Kind,
    pub quota: Option<Quota>,
}

impl Credentials {
    pub fn resolve(flags: &CredFlags) -> Result<Self> {
        let choice = choose(flags, |k| std::env::var(k).ok(), default_adc_path()?);
        let mut creds = Self::load(choice)?;
        let file_quota = match &creds.kind {
            Kind::User(user) => user.quota_project_id.clone(),
            Kind::Token(_) => None,
        };
        creds.quota = pick_quota(flags.quota_project.clone(), std::env::var(QUOTA_ENV).ok(), file_quota);
        Ok(creds)
    }

    pub fn load(choice: Choice) -> Result<Self> {
        let origin = &choice.origin;
        let kind = match &choice.source {
            Source::Token(token) => Kind::Token(non_empty_token(token, origin)?),
            Source::TokenFile(path) => {
                let text = std::fs::read_to_string(path).with_context(|| {
                    format!("reading the access token file {} (from {origin})", path.display())
                })?;
                Kind::Token(non_empty_token(&text, origin)?)
            }
            Source::Adc(path) => Kind::User(read_adc(path, origin)?),
        };
        Ok(Self { choice, kind, quota: None })
    }

    pub async fn access_token(&self, http: &reqwest::Client) -> Result<String> {
        match &self.kind {
            Kind::Token(token) => Ok(token.clone()),
            Kind::User(user) => oauth::refresh(http, user).await.with_context(|| self.describe()),
        }
    }

    pub fn quota_project(&self) -> Option<&str> {
        self.quota.as_ref().map(|q| q.project.as_str())
    }

    pub fn refreshable(&self) -> bool {
        matches!(self.kind, Kind::User(_))
    }

    /// Where these credentials came from, for error messages.
    pub fn describe(&self) -> String {
        let origin = &self.choice.origin;
        match &self.choice.source {
            Source::Token(_) => format!("access token from {origin}"),
            Source::TokenFile(path) => format!("access token in {} (from {origin})", path.display()),
            Source::Adc(path) => format!("credentials in {} (from {origin})", path.display()),
        }
    }

    /// What to do when Google rejects the access token.
    pub fn rejected_hint(&self) -> String {
        match &self.kind {
            Kind::Token(_) => format!("the {} is invalid or expired", self.describe()),
            Kind::User(_) => format!(
                "Google rejected the {}; run `gcs auth login` to sign in again",
                self.describe()
            ),
        }
    }
}

fn non_empty_token(raw: &str, origin: &Origin) -> Result<String> {
    let token = raw.trim();
    if token.is_empty() {
        bail!("the access token from {origin} is empty");
    }
    Ok(token.to_string())
}

fn read_adc(path: &Path, origin: &Origin) -> Result<AuthorizedUser> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && *origin == Origin::Default => bail!(
            "no credentials found. Run `gcs auth login`, or provide a token with \
             --access-token-file or ${TOKEN_ENV}"
        ),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {} (from {origin})", path.display()));
        }
    };
    parse_adc(&text).with_context(|| format!("{} (from {origin})", path.display()))
}

pub fn parse_adc(text: &str) -> Result<AuthorizedUser> {
    let value: serde_json::Value = serde_json::from_str(text).context("not valid JSON")?;
    let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if kind != "authorized_user" {
        bail!(
            "holds `{kind}` credentials, but Cloud Shell belongs to a user account; \
             use `authorized_user` credentials (run `gcs auth login`)"
        );
    }
    serde_json::from_value(value).context("incomplete `authorized_user` credentials")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    fn default() -> PathBuf {
        PathBuf::from("/default/adc.json")
    }

    fn all_env() -> Vec<(&'static str, &'static str)> {
        vec![(TOKEN_ENV, "env-token"), (TOKEN_FILE_ENV, "/env/token"), (ADC_ENV, "/env/adc.json")]
    }

    #[test]
    fn each_flag_beats_every_env_var() {
        let env = all_env();
        let cases = [
            (
                CredFlags { access_token: Some("flag-token".into()), ..Default::default() },
                Source::Token("flag-token".into()),
                Origin::Flag("--access-token"),
            ),
            (
                CredFlags { access_token_file: Some("/flag/token".into()), ..Default::default() },
                Source::TokenFile("/flag/token".into()),
                Origin::Flag("--access-token-file"),
            ),
            (
                CredFlags { credentials: Some("/flag/adc.json".into()), ..Default::default() },
                Source::Adc("/flag/adc.json".into()),
                Origin::Flag("--credentials"),
            ),
        ];
        for (flags, source, origin) in cases {
            assert_eq!(choose(&flags, env_of(&env), default()), Choice { source, origin });
        }
    }

    #[test]
    fn env_vars_in_order() {
        let flags = CredFlags::default();
        let env = all_env();
        assert_eq!(
            choose(&flags, env_of(&env), default()).source,
            Source::Token("env-token".into())
        );
        assert_eq!(
            choose(&flags, env_of(&env[1..]), default()).source,
            Source::TokenFile("/env/token".into())
        );
        assert_eq!(
            choose(&flags, env_of(&env[2..]), default()),
            Choice { source: Source::Adc("/env/adc.json".into()), origin: Origin::Env(ADC_ENV) }
        );
    }

    #[test]
    fn falls_back_to_default_path() {
        assert_eq!(
            choose(&CredFlags::default(), env_of(&[]), default()),
            Choice { source: Source::Adc(default()), origin: Origin::Default }
        );
    }

    #[test]
    fn empty_env_vars_count_as_unset() {
        let env = [(TOKEN_ENV, ""), (TOKEN_FILE_ENV, ""), (ADC_ENV, "/env/adc.json")];
        assert_eq!(
            choose(&CredFlags::default(), env_of(&env), default()).source,
            Source::Adc("/env/adc.json".into())
        );
    }

    #[test]
    fn quota_project_precedence() {
        let some = |s: &str| Some(s.to_string());
        let quota = |flag, env, file| pick_quota(flag, env, file).map(|q| q.project);
        assert_eq!(quota(some("flag"), some("env"), some("file")), some("flag"));
        assert_eq!(quota(None, some("env"), some("file")), some("env"));
        assert_eq!(quota(some(""), some(""), some("file")), some("file"));
        assert_eq!(quota(None, None, None), None);
        assert_eq!(pick_quota(None, some("env"), None).unwrap().origin, format!("${QUOTA_ENV}"));
    }

    #[test]
    fn token_file_is_trimmed_and_must_not_be_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let load = |path: &Path| {
            Credentials::load(Choice {
                source: Source::TokenFile(path.into()),
                origin: Origin::Flag("--access-token-file"),
            })
        };

        std::fs::write(&path, "  ya29.abc\n").unwrap();
        match load(&path).unwrap().kind {
            Kind::Token(t) => assert_eq!(t, "ya29.abc"),
            Kind::User(_) => panic!("expected a token"),
        }

        std::fs::write(&path, "\n").unwrap();
        let err = load(&path).err().unwrap();
        assert!(format!("{err:#}").contains("empty"), "{err:#}");
    }

    #[test]
    fn missing_default_file_suggests_login() {
        let dir = tempfile::tempdir().unwrap();
        let choice = Choice { source: Source::Adc(dir.path().join("nope.json")), origin: Origin::Default };
        let err = format!("{:#}", Credentials::load(choice).err().unwrap());
        assert!(err.contains("gcs auth login"), "{err}");
    }

    #[test]
    fn missing_explicit_file_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let choice = Choice { source: Source::Adc(path.clone()), origin: Origin::Env(ADC_ENV) };
        let err = format!("{:#}", Credentials::load(choice).err().unwrap());
        assert!(err.contains("nope.json") && err.contains(ADC_ENV), "{err}");
    }

    #[test]
    fn parses_authorized_user_with_quota_project() {
        let user = parse_adc(
            r#"{"type":"authorized_user","client_id":"id","client_secret":"s",
                "refresh_token":"r","quota_project_id":"proj","account":""}"#,
        )
        .unwrap();
        assert_eq!(user.refresh_token, "r");
        assert_eq!(user.quota_project_id.as_deref(), Some("proj"));
    }

    #[test]
    fn rejects_service_accounts() {
        let err = parse_adc(r#"{"type":"service_account","client_email":"x"}"#).unwrap_err();
        assert!(err.to_string().contains("service_account"), "{err}");
    }
}
