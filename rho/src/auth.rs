//! Anthropic subscription (Claude Pro/Max) OAuth: PKCE login, refresh and
//! credential storage.
//!
//! Uses the "copy code" flow (no local callback server): the user opens the
//! authorize URL, signs in, and pastes back the `code#state` shown by Anthropic.
//! Endpoints and client id match pi-ai's Anthropic OAuth implementation, and the
//! credential file uses pi's `auth.json` layout.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::provider::ProviderName;

crate::id_type! {
    /// An OAuth app's public client id.
    pub ClientId
}

pub const CLIENT_ID: ClientId = ClientId::from_static("9d1c250a-e61b-44d9-88ed-5944d1962f5e");
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
pub const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// pi's `auth.json`: credentials by provider key (`anthropic`,
/// `github-copilot`); other entries are kept as they are.
type AuthFile = Map<String, Value>;

/// A token or key. `Debug` hides it, so it does not end up in logs or
/// error messages; `expose` makes each use visible.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialKind {
    #[serde(rename = "oauth")]
    OAuth,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(rename = "type")]
    pub kind: CredentialKind,
    pub access: Secret,
    pub refresh: Secret,
    /// Expiry as unix millis (already reduced by a safety margin).
    pub expires: i64,
}

impl Credentials {
    fn is_expired(&self) -> bool {
        now_ms() >= self.expires
    }
}

pub struct Pkce {
    pub verifier: Secret,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
        Ok(Self::from_verifier(URL_SAFE_NO_PAD.encode(bytes)))
    }

    pub fn from_verifier(verifier: String) -> Self {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier: Secret(verifier),
            challenge,
        }
    }

    /// The URL the user opens to sign in. The verifier doubles as `state`,
    /// like pi does.
    pub fn authorize_url(&self) -> String {
        reqwest::Url::parse_with_params(
            AUTHORIZE_URL,
            [
                ("code", "true"),
                ("client_id", CLIENT_ID.as_str()),
                ("response_type", "code"),
                ("redirect_uri", REDIRECT_URI),
                ("scope", SCOPES),
                ("code_challenge", &self.challenge),
                ("code_challenge_method", "S256"),
                ("state", self.verifier.expose()),
            ],
        )
        .expect("static url is valid")
        .to_string()
    }
}

/// Parses what the user pasted: `code#state`, a redirect URL, or a bare code.
pub fn parse_code(input: &str) -> (String, Option<String>) {
    let input = input.trim();
    if let Ok(url) = reqwest::Url::parse(input) {
        let get = |k: &str| {
            url.query_pairs()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.into_owned())
        };
        if let Some(code) = get("code") {
            return (code, get("state"));
        }
    }
    match input.split_once('#') {
        Some((code, state)) => (code.to_string(), Some(state.to_string())),
        None => (input.to_string(), None),
    }
}

pub async fn exchange_code(
    http: &reqwest::Client,
    pasted: &str,
    pkce: &Pkce,
) -> Result<Credentials> {
    let (code, state) = parse_code(pasted);
    if code.is_empty() {
        bail!("missing authorization code");
    }
    let verifier = pkce.verifier.expose();
    if state.as_ref().is_some_and(|s| s != verifier) {
        bail!("OAuth state mismatch");
    }
    token_request(
        http,
        Grant::AuthorizationCode {
            code: &code,
            state: state.as_deref().unwrap_or(verifier),
            redirect_uri: REDIRECT_URI,
            code_verifier: verifier,
        },
    )
    .await
}

pub async fn refresh(http: &reqwest::Client, refresh_token: &Secret) -> Result<Credentials> {
    let refresh_token = refresh_token.expose();
    token_request(http, Grant::RefreshToken { refresh_token }).await
}

/// Body of a token request.
#[derive(Serialize)]
struct TokenRequest<'a> {
    client_id: &'a ClientId,
    #[serde(flatten)]
    grant: Grant<'a>,
}

#[derive(Serialize)]
#[serde(tag = "grant_type", rename_all = "snake_case")]
enum Grant<'a> {
    AuthorizationCode {
        code: &'a str,
        state: &'a str,
        redirect_uri: &'a str,
        code_verifier: &'a str,
    },
    RefreshToken {
        refresh_token: &'a str,
    },
}

async fn token_request(http: &reqwest::Client, grant: Grant<'_>) -> Result<Credentials> {
    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: Secret,
        refresh_token: Secret,
        expires_in: i64,
    }

    let body = TokenRequest {
        client_id: &CLIENT_ID,
        grant,
    };
    let resp = http.post(TOKEN_URL).json(&body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("token request failed: {status}: {text}");
    }
    let t: TokenResponse = serde_json::from_str(&text).context("invalid token response")?;
    Ok(Credentials {
        kind: CredentialKind::OAuth,
        access: t.access_token,
        refresh: t.refresh_token,
        expires: now_ms() + t.expires_in * 1000 - 5 * 60 * 1000,
    })
}

/// Credential file with lazy refresh. Other keys in the file are preserved,
/// so it can share pi's `~/.pi/agent/auth.json`.
pub struct AuthStore {
    path: PathBuf,
    http: reqwest::Client,
    /// Entry in the file, e.g. `anthropic`; also the `rho login` argument.
    key: ProviderName,
    lock: Mutex<()>,
}

impl AuthStore {
    pub fn new(path: PathBuf, http: reqwest::Client, key: ProviderName) -> Self {
        Self {
            path,
            http,
            key,
            lock: Mutex::new(()),
        }
    }

    /// `$RHO_AUTH_FILE`, else `$XDG_CONFIG_HOME/rho/auth.json`, else `~/.config/rho/auth.json`.
    pub fn default_path() -> PathBuf {
        if let Some(p) = std::env::var_os("RHO_AUTH_FILE") {
            return p.into();
        }
        crate::prompt::config_dir().join("auth.json")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a valid Anthropic access token, refreshing and persisting it if expired.
    pub async fn access_token(&self) -> Result<Secret> {
        let creds = self
            .fresh(async |http, creds| refresh(http, &creds.refresh).await)
            .await?;
        Ok(creds.access)
    }

    /// Valid credentials; expired ones are replaced by `refresh` and saved.
    pub async fn fresh(
        &self,
        refresh: impl AsyncFn(&reqwest::Client, &Credentials) -> Result<Credentials>,
    ) -> Result<Credentials> {
        let _guard = self.lock.lock().await;
        let key = &self.key;
        let creds = self.load()?.with_context(|| {
            format!(
                "not logged in (no {key} credentials in {}); run `rho login {key}`",
                self.path.display()
            )
        })?;
        if !creds.is_expired() {
            return Ok(creds);
        }
        let fresh = refresh(&self.http, &creds)
            .await
            .with_context(|| format!("refreshing {key} token"))?;
        self.save(&fresh)?;
        Ok(fresh)
    }

    pub fn load(&self) -> Result<Option<Credentials>> {
        let Some(entry) = self.read_file()?.remove(self.key.as_str()) else {
            return Ok(None);
        };
        let creds = serde_json::from_value(entry)
            .with_context(|| format!("parsing {} in {}", self.key, self.path.display()))?;
        Ok(Some(creds))
    }

    pub fn save(&self, creds: &Credentials) -> Result<()> {
        let mut file = self.read_file()?;
        file.insert(self.key.to_string(), serde_json::to_value(creds)?);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write a private temp file, then rename, so a crash never truncates the store.
        let tmp = self.path.with_extension("json.tmp");
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(serde_json::to_string_pretty(&file)?.as_bytes())?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn read_file(&self) -> Result<AuthFile> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => {
                serde_json::from_str(&s).with_context(|| format!("parsing {}", self.path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AuthFile::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::NAME as ANTHROPIC;

    #[test]
    fn pkce_challenge_is_s256() {
        // Reference: node -e 'crypto.createHash("sha256").update(v).digest("base64url")'
        let p = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFoEjXk".into());
        assert_eq!(p.challenge, "jCSJ3lETbbLQPeU7DcEBfnXmuYzJp04tkRw10FNxCq4");
    }

    #[test]
    fn authorize_url_has_required_params() {
        let p = Pkce::generate().unwrap();
        assert_eq!(format!("{:?}", p.verifier), "Secret(***)");
        let url = reqwest::Url::parse(&p.authorize_url()).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], CLIENT_ID.as_str());
        assert_eq!(q["redirect_uri"], REDIRECT_URI);
        assert_eq!(q["code_challenge"], p.challenge);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], p.verifier.expose());
        assert_eq!(q["code"], "true");
        assert!(q["scope"].contains("user:inference"));
    }

    #[test]
    fn parse_code_variants() {
        assert_eq!(
            parse_code(" abc#xyz \n"),
            ("abc".into(), Some("xyz".into()))
        );
        assert_eq!(parse_code("abc"), ("abc".into(), None));
        assert_eq!(
            parse_code("https://platform.claude.com/oauth/code/callback?code=c1&state=s1"),
            ("c1".into(), Some("s1".into()))
        );
    }

    #[tokio::test]
    async fn store_roundtrip_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, r#"{"openai":{"key":"x"}}"#).unwrap();
        let store = AuthStore::new(path.clone(), reqwest::Client::new(), ANTHROPIC);

        let creds = Credentials {
            kind: CredentialKind::OAuth,
            access: Secret::new("sk-ant-oat-test"),
            refresh: Secret::new("r"),
            expires: now_ms() + 60_000,
        };
        store.save(&creds).unwrap();

        assert_eq!(store.load().unwrap(), Some(creds));
        assert_eq!(
            store.access_token().await.unwrap(),
            Secret::new("sk-ant-oat-test")
        );
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["openai"]["key"], "x");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn missing_credentials_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(
            dir.path().join("none.json"),
            reqwest::Client::new(),
            ANTHROPIC,
        );
        let err = store.access_token().await.unwrap_err().to_string();
        assert!(err.contains("rho login anthropic"), "{err}");
    }
}
