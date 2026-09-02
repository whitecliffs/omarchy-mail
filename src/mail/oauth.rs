use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use native_tls::TlsConnector;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Google,
    Microsoft,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Google => "Google",
            Self::Microsoft => "Microsoft",
        }
    }

    fn client_id_env(self) -> &'static str {
        match self {
            Self::Google => "OMARCHY_MAIL_GOOGLE_CLIENT_ID",
            Self::Microsoft => "OMARCHY_MAIL_MICROSOFT_CLIENT_ID",
        }
    }

    fn authorization_endpoint(self) -> &'static str {
        match self {
            Self::Google => "https://accounts.google.com/o/oauth2/v2/auth",
            Self::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        }
    }

    fn token_endpoint(self) -> &'static str {
        match self {
            Self::Google => "https://oauth2.googleapis.com/token",
            Self::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        }
    }

    fn scope(self) -> &'static str {
        match self {
            Self::Google => "https://mail.google.com/",
            Self::Microsoft => {
                "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send"
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

impl OAuthTokens {
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at <= unix_now().saturating_add(60))
    }
}

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("OAuth is not available for this email provider")]
    UnsupportedProvider,
    #[error(
        "{provider} sign-in needs a registered desktop client ID; set {environment} or add it to ~/.config/omarchy-mail/oauth.json"
    )]
    ClientIdMissing {
        provider: &'static str,
        environment: &'static str,
    },
    #[error("could not prepare browser sign-in: {0}")]
    Preparation(#[from] std::io::Error),
    #[error("browser sign-in did not complete: {0}")]
    Authorization(String),
    #[error("secure token exchange failed: {0}")]
    Tls(#[from] native_tls::Error),
    #[error("token endpoint returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("token endpoint returned invalid data: {0}")]
    Response(#[from] serde_json::Error),
    #[error("token endpoint URL is invalid: {0}")]
    Url(#[from] url::ParseError),
}

#[derive(Debug)]
pub struct AuthorizationRequest {
    pub authorization_url: String,
    listener: TcpListener,
    provider: Provider,
    client_id: String,
    verifier: String,
    redirect_uri: String,
    state: String,
}

#[derive(Debug, Deserialize)]
struct Config {
    google_client_id: Option<String>,
    microsoft_client_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

pub fn provider_for_email(email: &str) -> Option<Provider> {
    let domain = email.rsplit_once('@')?.1.trim().to_ascii_lowercase();
    match domain.as_str() {
        "gmail.com" | "googlemail.com" => Some(Provider::Google),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" => Some(Provider::Microsoft),
        _ => None,
    }
}

pub fn configured_client_id(provider: Provider) -> Result<String, OAuthError> {
    if let Ok(value) = std::env::var(provider.client_id_env()) {
        if !value.trim().is_empty() {
            return Ok(value.trim().to_string());
        }
    }
    if let Some(path) = config_path()
        && let Ok(contents) = std::fs::read_to_string(path)
        && let Ok(config) = serde_json::from_str::<Config>(&contents)
    {
        let value = match provider {
            Provider::Google => config.google_client_id,
            Provider::Microsoft => config.microsoft_client_id,
        };
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            return Ok(value.trim().to_string());
        }
    }
    Err(OAuthError::ClientIdMissing {
        provider: provider.label(),
        environment: provider.client_id_env(),
    })
}

pub fn begin(email: &str) -> Result<AuthorizationRequest, OAuthError> {
    let provider = provider_for_email(email).ok_or(OAuthError::UnsupportedProvider)?;
    let client_id = configured_client_id(provider)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/oauth/callback");
    let verifier = random_urlsafe(48)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(32)?;
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query
        .append_pair("client_id", &client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", provider.scope())
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("login_hint", email.trim());
    Ok(AuthorizationRequest {
        authorization_url: format!("{}?{}", provider.authorization_endpoint(), query.finish()),
        listener,
        provider,
        client_id,
        verifier,
        redirect_uri,
        state,
    })
}

pub fn complete(request: AuthorizationRequest) -> Result<OAuthTokens, OAuthError> {
    let mut stream = wait_for_callback(request.listener)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let callback = read_callback_target(&mut stream)?;
    let callback_url = Url::parse(&format!("http://localhost{callback}"))?;
    let query = callback_url
        .query_pairs()
        .into_owned()
        .collect::<std::collections::HashMap<_, _>>();
    if let Some(error) = query.get("error") {
        write_callback_response(&mut stream, false)?;
        let detail = query
            .get("error_description")
            .map(|description| format!(": {description}"))
            .unwrap_or_default();
        return Err(OAuthError::Authorization(format!("{error}{detail}")));
    }
    if query.get("state").map(String::as_str) != Some(request.state.as_str()) {
        write_callback_response(&mut stream, false)?;
        return Err(OAuthError::Authorization(
            "the browser response state did not match this sign-in request".into(),
        ));
    }
    let Some(code) = query.get("code") else {
        write_callback_response(&mut stream, false)?;
        return Err(OAuthError::Authorization(
            "the browser did not return an authorization code".into(),
        ));
    };
    write_callback_response(&mut stream, true)?;
    exchange_code(
        request.provider,
        &request.client_id,
        &request.redirect_uri,
        &request.verifier,
        code,
    )
}

pub fn refresh_access_token(
    provider: Provider,
    client_id: &str,
    refresh_token: &str,
) -> Result<OAuthTokens, OAuthError> {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.append_pair("client_id", client_id)
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token);
    parse_token_response(http_post_form(provider.token_endpoint(), &form.finish())?)
}

fn exchange_code(
    provider: Provider,
    client_id: &str,
    redirect_uri: &str,
    verifier: &str,
    code: &str,
) -> Result<OAuthTokens, OAuthError> {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.append_pair("client_id", client_id)
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_verifier", verifier);
    parse_token_response(http_post_form(provider.token_endpoint(), &form.finish())?)
}

fn parse_token_response(body: String) -> Result<OAuthTokens, OAuthError> {
    let response: TokenResponse = serde_json::from_str(&body)?;
    if response.access_token.trim().is_empty() {
        return Err(OAuthError::Authorization(
            "the provider returned an empty access token".into(),
        ));
    }
    let expires_at = response
        .expires_in
        .map(|seconds| unix_now().saturating_add(seconds.min(i64::MAX as u64) as i64));
    Ok(OAuthTokens {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        expires_at,
    })
}

fn http_post_form(endpoint: &str, form: &str) -> Result<String, OAuthError> {
    let endpoint = Url::parse(endpoint)?;
    if endpoint.scheme() != "https" {
        return Err(OAuthError::Authorization(
            "the token endpoint did not use HTTPS".into(),
        ));
    }
    let host = endpoint
        .host_str()
        .ok_or_else(|| OAuthError::Authorization("the token endpoint has no host".into()))?;
    let port = endpoint.port_or_known_default().unwrap_or(443);
    let stream = TcpStream::connect((host, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;
    let connector = TlsConnector::builder().build()?;
    let mut stream = connector
        .connect(host, stream)
        .map_err(|error| OAuthError::Authorization(format!("TLS handshake failed: {error:?}")))?;
    let path = endpoint
        .path_segments()
        .map(|segments| format!("/{}", segments.collect::<Vec<_>>().join("/")))
        .unwrap_or_else(|| "/".into());
    let path = if let Some(query) = endpoint.query() {
        format!("{path}?{query}")
    } else {
        path
    };
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{form}",
        form.len()
    );
    stream.write_all(request.as_bytes())?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    parse_http_response(&response)
}

fn parse_http_response(response: &[u8]) -> Result<String, OAuthError> {
    let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err(OAuthError::Authorization(
            "the token endpoint returned an incomplete HTTP response".into(),
        ));
    };
    let headers = String::from_utf8_lossy(&response[..header_end]);
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    let body = &response[header_end + 4..];
    let body = if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        decode_chunked_body(body)
    } else {
        body.to_vec()
    };
    let body = String::from_utf8_lossy(&body).into_owned();
    if !(200..300).contains(&status) {
        return Err(OAuthError::Http {
            status,
            body: compact_error_body(&body),
        });
    }
    Ok(body)
}

fn decode_chunked_body(mut body: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    while let Some(line_end) = body.windows(2).position(|window| window == b"\r\n") {
        let Ok(size) = usize::from_str_radix(
            String::from_utf8_lossy(&body[..line_end])
                .split(';')
                .next()
                .unwrap_or_default()
                .trim(),
            16,
        ) else {
            break;
        };
        body = &body[line_end + 2..];
        if size == 0 || body.len() < size + 2 {
            break;
        }
        decoded.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
    decoded
}

fn wait_for_callback(listener: TcpListener) -> Result<TcpStream, OAuthError> {
    for _ in 0..300 {
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(OAuthError::Authorization(
        "timed out waiting for the browser response".into(),
    ))
}

fn read_callback_target(stream: &mut TcpStream) -> Result<String, OAuthError> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    while request.len() < 32 * 1024 {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request_text = String::from_utf8_lossy(&request);
    let line = request_text
        .lines()
        .next()
        .ok_or_else(|| OAuthError::Authorization("the browser sent no callback request".into()))?;
    line.split_whitespace()
        .nth(1)
        .map(ToOwned::to_owned)
        .ok_or_else(|| OAuthError::Authorization("the browser callback was malformed".into()))
}

fn write_callback_response(stream: &mut TcpStream, success: bool) -> Result<(), OAuthError> {
    let body = if success {
        "<!doctype html><title>Omarchy Mail</title><p>Sign-in complete. You can close this tab.</p>"
    } else {
        "<!doctype html><title>Omarchy Mail</title><p>Sign-in could not be completed. Return to Omarchy Mail for details.</p>"
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    stream.flush()?;
    Ok(())
}

fn random_urlsafe(length: usize) -> Result<String, OAuthError> {
    let mut bytes = vec![0_u8; length];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn compact_error_body(body: &str) -> String {
    body.chars()
        .filter(|character| !character.is_control())
        .take(240)
        .collect()
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|home| home.join("omarchy-mail").join("oauth.json"))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_consumer_providers() {
        assert_eq!(provider_for_email("Jim@gmail.com"), Some(Provider::Google));
        assert_eq!(
            provider_for_email("jim@outlook.com"),
            Some(Provider::Microsoft)
        );
        assert_eq!(provider_for_email("jim@example.com"), None);
    }

    #[test]
    fn expires_tokens_with_a_small_clock_skew_window() {
        let token = OAuthTokens {
            access_token: "token".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(unix_now() + 30),
        };
        assert!(token.is_expired());
    }

    #[test]
    fn builds_pkce_challenges_without_padding() {
        let verifier = "a-secure-verifier";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert!(!challenge.contains('='));
        assert!(!challenge.is_empty());
    }
}
