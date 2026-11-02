use crate::catalog::{Method as Verb, Op, Out};
use crate::{Credentials, Error, Result, Store};
use reqwest::{Method, blocking::Response};
use serde_json::Value;
use std::{io::Read, time::Duration};
use url::Url;
use zeroize::Zeroizing;

pub const DEFAULT_BASE_URL: &str = "https://portal.cooperalianca.com.br/agenciavirtual";
const LIMIT: u64 = 12 * 1024 * 1024;
const PDF_LIMIT: u64 = 64 * 1024 * 1024;

/// What the portal's own `EmpConfig.js` tells the web app: the backend API root and the public
/// external token it sends as `use-token-externo`. Both are read at runtime, never committed.
struct Config {
    api: Url,
    external_token: Zeroizing<String>,
}
/// A call result: decoded JSON `Content`, or PDF bytes for document operations.
pub enum Output {
    Json(Value),
    Pdf(Vec<u8>),
}
/// A client is deliberately used through &mut self: one session renewal at a time.
/// A persistent store holds a process lock throughout the client's lifetime.
pub struct Client {
    offline: bool,
    base: Url,
    http: reqwest::blocking::Client,
    config: Option<Config>,
    token: Option<Zeroizing<String>>,
    store: Option<Store>,
    credentials: Option<Credentials>,
}
impl Client {
    pub fn normalize_base(base: &str) -> Result<Url> {
        let mut u = Url::parse(base).map_err(|_| Error::Validation("invalid base URL"))?;
        if u.host_str().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
        {
            return Err(Error::Validation(
                "base URL must have a host and no credentials, query, or fragment",
            ));
        }
        let loopback = matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if u.scheme() != "https" && !(u.scheme() == "http" && loopback) {
            return Err(Error::Validation(
                "HTTPS is required except for localhost tests",
            ));
        }
        u.set_path(&format!("{}/", u.path().trim_end_matches('/')));
        Ok(u)
    }
    /// Construct a client that refuses all HTTP calls, including authentication.
    pub fn offline(base: &str) -> Result<Self> {
        let mut client = Self::new(base)?;
        client.offline = true;
        Ok(client)
    }
    pub fn new(base: &str) -> Result<Self> {
        Self::build(base, None, None)
    }
    pub fn from_store(base: &str, store: Store) -> Result<Self> {
        Self::build(base, Some(store), None)
    }
    pub fn with_credentials(base: &str, creds: Credentials) -> Result<Self> {
        Self::build(base, None, Some(creds))
    }
    fn build(base: &str, store: Option<Store>, credentials: Option<Credentials>) -> Result<Self> {
        let base = Self::normalize_base(base)?;
        let token = match &store {
            Some(s) => match s.load("session.enc", base.as_str())? {
                Some(data) => Some(Zeroizing::new(
                    String::from_utf8(data.to_vec()).map_err(|_| Error::Crypto)?,
                )),
                None => None,
            },
            None => None,
        };
        let origin = base.origin();
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!(
                "cooperalianca-library-rust/",
                env!("CARGO_PKG_VERSION")
            ))
            .redirect(reqwest::redirect::Policy::custom(move |a| {
                if a.previous().len() >= 5 {
                    return a.error("redirect limit");
                }
                if a.url().origin() != origin {
                    return a.error("cross-origin redirect refused");
                }
                a.follow()
            }))
            .build()
            .map_err(|_| Error::Transport)?;
        Ok(Self {
            offline: false,
            base,
            http,
            config: None,
            token,
            store,
            credentials,
        })
    }
    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }
    fn save_session(&self) -> Result<()> {
        if let (Some(s), Some(t)) = (&self.store, &self.token) {
            s.save("session.enc", self.base.as_str(), t.as_bytes())?;
        }
        Ok(())
    }
    pub fn forget_session(&mut self) -> Result<()> {
        self.token = None;
        if let Some(s) = &self.store {
            s.clear_session()?;
        }
        Ok(())
    }
    fn has_credentials(&self) -> bool {
        self.credentials.is_some()
            || self
                .store
                .as_ref()
                .is_some_and(|s| s.credentials(self.base.as_str()).is_ok())
    }
    fn response_bytes(mut response: Response, limit: u64) -> Result<(u16, Vec<u8>)> {
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        (&mut response)
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Transport)?;
        if bytes.len() as u64 > limit {
            return Err(Error::TooLarge);
        }
        Ok((status, bytes))
    }
    fn send(
        &self,
        method: Method,
        u: Url,
        body: Option<&[u8]>,
        limit: u64,
    ) -> Result<(u16, Vec<u8>)> {
        if self.offline {
            return Err(Error::Offline);
        }
        // Only safe GETs retry transient failures. Authentication and writes are sent once.
        let attempts = if method == Method::GET { 3 } else { 1 };
        for attempt in 0..attempts {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(200 << (attempt - 1)));
            }
            let mut req = self
                .http
                .request(method.clone(), u.clone())
                // The web app (axios) sends this; the portal answers 406 to a bare application/json.
                .header("Accept", "application/json, text/plain, */*");
            if let Some(c) = &self.config {
                req = req.header("use-token-externo", c.external_token.as_str());
            }
            if let Some(t) = &self.token {
                req = req.bearer_auth(t.as_str());
            }
            if let Some(b) = body {
                req = req
                    .header("Content-Type", "application/json")
                    .body(b.to_vec());
            }
            let response = match req.send() {
                Ok(r) => r,
                Err(_) if attempt + 1 < attempts => continue,
                Err(_) => return Err(Error::Transport),
            };
            let status = response.status().as_u16();
            if matches!(status, 429 | 502 | 503 | 504) && attempt + 1 < attempts {
                // Respect Retry-After seconds up to a bounded wait. Long waits are returned to caller.
                if let Some(wait) = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                {
                    if wait > 5 {
                        return Err(Error::Http(status));
                    }
                    std::thread::sleep(Duration::from_secs(wait));
                }
                continue;
            }
            return Self::response_bytes(response, limit);
        }
        Err(Error::Transport)
    }
    /// Reads the backend root and external token the web app itself loads. Fails closed.
    fn load_config(&mut self) -> Result<()> {
        if self.config.is_some() {
            return Ok(());
        }
        let u = self
            .base
            .join("EmpConfig.js")
            .map_err(|_| Error::Validation("invalid endpoint"))?;
        let (status, bytes) = self.send(Method::GET, u, None, 1024 * 1024)?;
        if !(200..300).contains(&status) {
            return Err(Error::Http(status));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| Error::Parse)?;
        let api = config_value(text, "USE_BACKEND_URL_PROD").ok_or(Error::Parse)?;
        let token = config_value(text, "TOKEN_EXTERNO").ok_or(Error::Parse)?;
        let api = Self::normalize_base(&api)?;
        if api.origin() != self.base.origin() {
            return Err(Error::Validation("backend must share the portal origin"));
        }
        if token.is_empty()
            || token.len() > 8192
            || !token.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'=' | b'+' | b'/')
            })
        {
            return Err(Error::Parse);
        }
        self.config = Some(Config {
            api,
            external_token: Zeroizing::new(token),
        });
        Ok(())
    }
    /// Decodes the `{Success, Content, Message}` envelope. Only `Content` is returned.
    fn decode(status: u16, bytes: &[u8]) -> Result<Value> {
        if matches!(status, 401 | 403) {
            return Err(Error::Unauthenticated);
        }
        let env: Option<Value> = serde_json::from_slice(bytes).ok();
        if !(200..300).contains(&status) {
            return Err(match env {
                Some(v) if v.get("Success") == Some(&Value::Bool(false)) && status == 400 => {
                    Error::Api
                }
                _ => Error::Http(status),
            });
        }
        let env = env.ok_or(Error::Parse)?;
        match env.get("Success") {
            Some(Value::Bool(true)) => {}
            Some(Value::Bool(false)) => return Err(Error::Api),
            _ => return Err(Error::Parse),
        }
        env.get("Content").cloned().ok_or(Error::Parse)
    }
    fn api_url(&self, path: &str) -> Result<Url> {
        let api = &self.config.as_ref().ok_or(Error::Parse)?.api;
        let u = api
            .join(path)
            .map_err(|_| Error::Validation("invalid endpoint"))?;
        if u.origin() != api.origin() || !u.username().is_empty() || u.password().is_some() {
            return Err(Error::Validation("cross-origin endpoint refused"));
        }
        Ok(u)
    }
    /// Login once using enrolled credentials. No recursion and no password retries.
    pub fn authenticate(&mut self) -> Result<()> {
        if self.offline {
            return Err(Error::Offline);
        }
        self.load_config()?;
        let loaded;
        let creds = match &self.credentials {
            Some(c) => c,
            None => {
                loaded = self
                    .store
                    .as_ref()
                    .ok_or(Error::Unauthenticated)?
                    .credentials(self.base.as_str())?;
                &loaded
            }
        };
        let body = Zeroizing::new(
            serde_json::to_vec(&serde_json::json!({
                "EmailInscricao": creds.username,
                "Senha": creds.password,
            }))
            .map_err(|_| Error::Parse)?,
        );
        let u = self.api_url("Auth")?;
        self.token = None;
        let (status, bytes) = self.send(Method::POST, u, Some(&body), LIMIT)?;
        let content = match Self::decode(status, &bytes) {
            Ok(c) => c,
            // Wrong credentials, a locked account, and a rejected request all end the attempt.
            Err(Error::Api | Error::Unauthenticated) => return Err(Error::Unauthenticated),
            Err(e) => return Err(e),
        };
        if content.get("PossuiAuthDoisFatores") == Some(&Value::Bool(true)) {
            return Err(Error::TwoFactor);
        }
        if content
            .get("AceitePoliticaPrivacidade")
            .and_then(Value::as_str)
            != Some("S")
        {
            return Err(Error::PrivacyPolicy);
        }
        let token = content
            .get("Token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or(Error::Parse)?;
        self.token = Some(Zeroizing::new(token.to_owned()));
        self.save_session()
    }
    /// Runs one catalog operation. Callers pass already validated query and body values.
    pub fn execute(
        &mut self,
        op: &Op,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Output> {
        if self.offline {
            return Err(Error::Offline);
        }
        self.load_config()?;
        if self.token.is_none() && self.has_credentials() {
            self.authenticate()?;
        }
        let mut u = self.api_url(path)?;
        if !query.is_empty() {
            let mut pairs = u.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }
        let method = match op.method {
            Verb::Get => Method::GET,
            Verb::Post => Method::POST,
            Verb::Put => Method::PUT,
        };
        let payload = match body {
            Some(b) => Some(serde_json::to_vec(b).map_err(|_| Error::Parse)?),
            None => None,
        };
        let limit = if op.out == Out::Pdf { PDF_LIMIT } else { LIMIT };
        let mut renewed = false;
        loop {
            let (status, bytes) =
                self.send(method.clone(), u.clone(), payload.as_deref(), limit)?;
            let result = match op.out {
                Out::Json => Self::decode(status, &bytes).map(Output::Json),
                Out::Pdf => {
                    if matches!(status, 401 | 403) {
                        Err(Error::Unauthenticated)
                    } else if !(200..300).contains(&status) {
                        Err(Error::Http(status))
                    } else if !bytes.starts_with(b"%PDF-") {
                        Err(Error::Parse)
                    } else {
                        Ok(Output::Pdf(bytes))
                    }
                }
            };
            match result {
                // One renewal per call. A 401 means the request was rejected, not executed.
                Err(Error::Unauthenticated) if !renewed && self.has_credentials() => {
                    renewed = true;
                    self.authenticate()?;
                }
                other => return other,
            }
        }
    }
}
fn config_value(text: &str, key: &str) -> Option<String> {
    let rest = &text[text.find(key)? + key.len()..];
    let rest = rest.trim_start_matches(['"', '\'']).trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let rest = &rest[1..];
    Some(rest[..rest.find(quote)?].to_owned())
}
pub(crate) fn required(s: &str) -> Result<()> {
    if s.trim().is_empty() {
        Err(Error::Validation("required string is empty"))
    } else {
        Ok(())
    }
}
