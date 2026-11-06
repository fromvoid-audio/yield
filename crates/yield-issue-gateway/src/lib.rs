//! Yield feedback issue gateway client and OS keyring token storage.
//!
//! This crate does not automatically retry issue creation. A transport error
//! after submission has an unknown outcome and requires user reconciliation.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Read;
use std::time::Duration;
use url::Url;
use uuid::Uuid;

const KEYRING_SERVICE: &str = "yield";
const KEYRING_KEY: &str = "yield_github_issue_token";
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// Opaque gateway session credential. Debug output never reveals it.
#[derive(Clone)]
pub struct SecretToken(String);

impl SecretToken {
    /// Construct a token returned by a validated gateway response.
    pub fn new(value: String) -> Option<Self> {
        let valid = value.len() >= 32
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        valid.then_some(Self(value))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken([REDACTED])")
    }
}

/// Errors from the OS keyring. No backend error text or credential is exposed.
#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    /// Keyring access failed.
    #[error("Secure token storage is unavailable")]
    Unavailable,
}

/// Local token storage for the Yield feedback connection.
pub struct TokenStore;

impl TokenStore {
    fn entry() -> Result<keyring::Entry, TokenStoreError> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_KEY).map_err(|_| TokenStoreError::Unavailable)
    }

    /// Load a previously connected gateway session, if one exists.
    pub fn load() -> Result<Option<SecretToken>, TokenStoreError> {
        let entry = Self::entry()?;
        match entry.get_password() {
            Ok(value) => SecretToken::new(value)
                .ok_or(TokenStoreError::Unavailable)
                .map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(TokenStoreError::Unavailable),
        }
    }

    /// Store and read back a gateway session in the OS keyring.
    pub fn store(token: &SecretToken) -> Result<(), TokenStoreError> {
        let entry = Self::entry()?;
        entry
            .set_password(token.expose())
            .map_err(|_| TokenStoreError::Unavailable)?;
        match Self::load()? {
            Some(stored) if stored.expose() == token.expose() => Ok(()),
            _ => Err(TokenStoreError::Unavailable),
        }
    }

    /// Disconnect by removing the session credential from the OS keyring.
    pub fn delete() -> Result<(), TokenStoreError> {
        let entry = Self::entry()?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(TokenStoreError::Unavailable),
        }
    }
}

/// A random identifier binding browser OAuth completion to one app request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthRequestId(String);

impl AuthRequestId {
    /// Create an unpredictable request identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4().simple().to_string())
    }

    /// Read the non-secret request identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for AuthRequestId {
    fn default() -> Self {
        Self::new()
    }
}

/// Gateway errors are safe for UI display and never include response bodies or tokens.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// No deployed gateway endpoint is configured.
    #[error("Feedback service is not configured")]
    Unconfigured,
    /// The endpoint is malformed or insecure.
    #[error("Feedback service URL is invalid")]
    InvalidUrl,
    /// The gateway or network is unavailable.
    #[error("Feedback service is unavailable")]
    Unavailable,
    /// Browser authorization was rejected or expired.
    #[error("GitHub connection is unavailable or expired")]
    Unauthorized,
    /// The user-provided issue is invalid.
    #[error("Issue title or body is invalid")]
    InvalidIssue,
    /// GitHub rejected the issue content.
    #[error("GitHub rejected the issue content")]
    Rejected,
    /// The gateway refused the request before attempting an issue POST.
    #[error("Too many feedback requests. Try again in a minute.")]
    RateLimited,
    /// The POST may have succeeded; check before retrying.
    #[error("Submission outcome unknown; check the feedback repository before retrying")]
    UnknownOutcome,
}

/// A confirmed issue returned by the gateway.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedIssue {
    /// URL of the created GitHub issue.
    pub url: Url,
    /// Numeric GitHub issue identifier.
    pub number: u64,
}

#[derive(Deserialize)]
struct PollResponse {
    ok: bool,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct CreateResponse {
    ok: bool,
    issue_url: String,
    number: u64,
}

#[derive(Serialize)]
struct CreateRequest<'a> {
    title: &'a str,
    body: &'a str,
}

/// Blocking client intended for a dedicated background worker, never the UI or audio thread.
#[derive(Clone)]
pub struct GatewayClient {
    base: Url,
    agent: ureq::Agent,
}

impl GatewayClient {
    /// Construct from the deployment endpoint in `YIELD_ISSUE_GATEWAY_URL`.
    pub fn from_environment() -> Result<Self, GatewayError> {
        let value =
            std::env::var("YIELD_ISSUE_GATEWAY_URL").map_err(|_| GatewayError::Unconfigured)?;
        Self::new(&value)
    }

    /// Construct from an explicit endpoint for a configured deployment or local test.
    pub fn new(base_url: &str) -> Result<Self, GatewayError> {
        let base = Url::parse(base_url).map_err(|_| GatewayError::InvalidUrl)?;
        let local_http =
            base.scheme() == "http" && matches!(base.host_str(), Some("localhost" | "127.0.0.1"));
        if !(base.scheme() == "https" || local_http)
            || base.username() != ""
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(GatewayError::InvalidUrl);
        }
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(15))
            .timeout_write(Duration::from_secs(15))
            .build();
        Ok(Self { base, agent })
    }

    fn endpoint(&self, path: &str) -> Result<Url, GatewayError> {
        self.base.join(path).map_err(|_| GatewayError::InvalidUrl)
    }

    /// URL to open in the user's browser to begin GitHub OAuth connection.
    pub fn auth_start_url(&self, request_id: &AuthRequestId) -> Result<Url, GatewayError> {
        let mut url = self.endpoint("auth/start")?;
        url.query_pairs_mut()
            .append_pair("requestId", request_id.as_str());
        Ok(url)
    }

    /// Poll a pending browser connection. Returns `None` while authorization continues.
    pub fn poll(&self, request_id: &AuthRequestId) -> Result<Option<SecretToken>, GatewayError> {
        let mut url = self.endpoint("auth/poll")?;
        url.query_pairs_mut()
            .append_pair("requestId", request_id.as_str());
        let response = match self.agent.get(url.as_str()).call() {
            Ok(response) => response,
            Err(ureq::Error::Status(202, _)) => return Ok(None),
            Err(ureq::Error::Status(401 | 403, _)) => return Err(GatewayError::Unauthorized),
            Err(ureq::Error::Status(429, _)) => return Err(GatewayError::RateLimited),
            Err(_) => return Err(GatewayError::Unavailable),
        };
        let parsed: PollResponse = read_json(response).map_err(|_| GatewayError::Unavailable)?;
        if !parsed.ok {
            return Ok(None);
        }
        parsed
            .session_id
            .and_then(SecretToken::new)
            .map(Some)
            .ok_or(GatewayError::Unavailable)
    }

    /// Submit exactly one issue POST. Uncertain results are never retried automatically.
    pub fn create_issue(
        &self,
        token: &SecretToken,
        title: &str,
        body: &str,
    ) -> Result<CreatedIssue, GatewayError> {
        let title = title.trim();
        if title.len() < 3 || title.len() > 200 || body.len() > 60_000 {
            return Err(GatewayError::InvalidIssue);
        }
        let url = self.endpoint("issue")?;
        let request = CreateRequest { title, body };
        let response = match self
            .agent
            .post(url.as_str())
            .set("Authorization", &format!("Bearer {}", token.expose()))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .send_json(request)
        {
            Ok(response) => response,
            Err(ureq::Error::Status(401 | 403, _)) => return Err(GatewayError::Unauthorized),
            Err(ureq::Error::Status(400 | 422, _)) => return Err(GatewayError::Rejected),
            Err(ureq::Error::Status(429, _)) => return Err(GatewayError::RateLimited),
            Err(_) => return Err(GatewayError::UnknownOutcome),
        };
        let parsed: CreateResponse =
            read_json(response).map_err(|_| GatewayError::UnknownOutcome)?;
        let url = Url::parse(&parsed.issue_url).map_err(|_| GatewayError::UnknownOutcome)?;
        if !parsed.ok
            || parsed.number == 0
            || url.scheme() != "https"
            || url.host_str() != Some("github.com")
            || url.port().is_some()
            || url.username() != ""
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path_segments().map(|segments| segments.collect::<Vec<_>>()),
                Some(segments) if segments.len() == 4
                    && !segments[0].is_empty()
                    && !segments[1].is_empty()
                    && segments[2] == "issues"
                    && segments[3] == parsed.number.to_string())
        {
            return Err(GatewayError::UnknownOutcome);
        }
        Ok(CreatedIssue {
            url,
            number: parsed.number,
        })
    }
}

fn read_json<T: for<'a> Deserialize<'a>>(response: ureq::Response) -> Result<T, ()> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(());
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_token_store_selects_secret_service() {
        let entry = TokenStore::entry().expect("valid credential locator");
        assert!(
            entry
                .get_credential()
                .is::<keyring::secret_service::SsCredential>()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires explicit Linux Secret Service qualification"]
    fn linux_secret_service_probe() {
        let step = std::env::var("YIELD_KEYRING_TEST_STEP").expect("explicit probe step");
        if step == "unavailable" {
            assert!(matches!(
                TokenStore::load(),
                Err(TokenStoreError::Unavailable)
            ));
            return;
        }

        let account = std::env::var("YIELD_KEYRING_TEST_ACCOUNT").expect("unique probe account");
        assert!(
            account
                .strip_prefix("yield-qa-")
                .is_some_and(|suffix| Uuid::parse_str(suffix).is_ok())
        );
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account).expect("valid probe locator");
        let token = "yield_linux_keyring_fixture_only_0000";
        match step.as_str() {
            "store" => assert!(entry.set_password(token).is_ok()),
            "load" => assert!(entry.get_password().is_ok_and(|value| value == token)),
            "delete" => assert!(entry.delete_credential().is_ok()),
            "absent" => assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry))),
            _ => panic!("unknown probe step"),
        }
    }

    fn serve_once(status: &str, body: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("local test listener");
        let address = format!("http://{}/", listener.local_addr().unwrap());
        let status = status.to_owned();
        let task = thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("one client request");
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("bounded test read");
            let mut request_bytes = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let size = socket.read(&mut chunk).expect("request bytes");
                assert!(size > 0, "request ended before its body");
                request_bytes.extend_from_slice(&chunk[..size]);
                assert!(request_bytes.len() <= 70_000, "test request exceeded bound");
                if let Some(header_end) = request_bytes
                    .windows(4)
                    .position(|bytes| bytes == b"\r\n\r\n")
                {
                    let headers = String::from_utf8_lossy(&request_bytes[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        })
                        .map(|(_, value)| value.trim().parse::<usize>().expect("content length"))
                        .unwrap_or(0);
                    if request_bytes.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&request_bytes).into_owned();
            write!(
                socket,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("response bytes");
            request
        });
        (address, task)
    }

    #[test]
    fn token_debug_and_gateway_errors_never_expose_secret() {
        let token = SecretToken::new("s".repeat(43)).expect("valid token");
        assert_eq!(format!("{token:?}"), "SecretToken([REDACTED])");
        assert!(!format!("{}", GatewayError::UnknownOutcome).contains(token.expose()));
    }

    #[test]
    fn endpoint_requires_https_or_loopback_and_no_embedded_credentials() {
        assert!(GatewayClient::new("https://yield-gateway.example/").is_ok());
        assert!(GatewayClient::new("http://localhost:8787/").is_ok());
        assert!(GatewayClient::new("http://example.com/").is_err());
        assert!(GatewayClient::new("https://secret@example.com/").is_err());
    }

    #[test]
    fn auth_url_contains_unpredictable_request_id() {
        let client = GatewayClient::new("https://yield-gateway.example/").unwrap();
        let first = AuthRequestId::new();
        let second = AuthRequestId::new();
        assert_ne!(first, second);
        let url = client.auth_start_url(&first).unwrap();
        assert_eq!(url.path(), "/auth/start");
        assert!(url.as_str().contains(first.as_str()));
    }

    #[test]
    fn post_server_error_has_unknown_outcome_without_automatic_retry() {
        let (address, server) =
            serve_once("503 Service Unavailable", "{\"error\":\"secret-token\"}");
        let client = GatewayClient::new(&address).unwrap();
        let token = SecretToken::new("s".repeat(43)).unwrap();
        let result = client.create_issue(&token, "Bug: sample", "Details");
        assert!(matches!(result, Err(GatewayError::UnknownOutcome)));
        let request = server.join().expect("one server request");
        assert!(request.starts_with("POST /issue HTTP/1.1"));
        assert!(!format!("{result:?}").contains("secret-token"));
    }

    #[test]
    fn rate_limit_is_a_known_rejection_for_poll_and_post() {
        let (address, server) = serve_once("429 Too Many Requests", "{}");
        let client = GatewayClient::new(&address).unwrap();
        let result = client.poll(&AuthRequestId::new());
        assert!(matches!(result, Err(GatewayError::RateLimited)));
        assert!(server.join().unwrap().starts_with("GET /auth/poll?"));

        let (address, server) = serve_once("429 Too Many Requests", "{}");
        let client = GatewayClient::new(&address).unwrap();
        let token = SecretToken::new("s".repeat(43)).unwrap();
        let result = client.create_issue(&token, "Bug: sample", "Details");
        assert!(matches!(result, Err(GatewayError::RateLimited)));
        assert!(server.join().unwrap().starts_with("POST /issue HTTP/1.1"));
    }

    #[test]
    fn confirmed_issue_response_is_returned_without_reposting() {
        let (address, server) = serve_once(
            "200 OK",
            "{\"ok\":true,\"issue_url\":\"https://github.com/PORTALSURFER/yield-feedback/issues/7\",\"number\":7}",
        );
        let client = GatewayClient::new(&address).unwrap();
        let token = SecretToken::new("s".repeat(43)).unwrap();
        let created = client
            .create_issue(&token, "Bug: sample", "Details")
            .unwrap();
        assert_eq!(created.number, 7);
        assert_eq!(
            created.url.as_str(),
            "https://github.com/PORTALSURFER/yield-feedback/issues/7"
        );
        let request = server.join().expect("one server request");
        assert!(request.starts_with("POST /issue HTTP/1.1"));
    }

    #[test]
    fn unrelated_github_url_is_not_a_confirmed_issue() {
        let (address, server) = serve_once(
            "200 OK",
            "{\"ok\":true,\"issue_url\":\"https://github.com/PORTALSURFER/yield-feedback/issues/7?unexpected=1\",\"number\":7}",
        );
        let client = GatewayClient::new(&address).unwrap();
        let token = SecretToken::new("s".repeat(43)).unwrap();
        assert!(matches!(
            client.create_issue(&token, "Bug: sample", "Details"),
            Err(GatewayError::UnknownOutcome)
        ));
        server.join().expect("one server request");
    }
}
