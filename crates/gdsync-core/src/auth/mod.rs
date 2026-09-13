use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;

// Google OAuth 2.0 endpoints
const GOOGLE_AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";
const REDIRECT_PORT: u16 = 8085;
const REDIRECT_URI: &str = "http://127.0.0.1:8085/oauth2callback";

// Fallback client ID placeholder (users can configure their own in config.toml or CLI)
pub const DEFAULT_CLIENT_ID: &str = "841890352233-0m8t7f4r2s1d60vceg100r7908t32q37.apps.googleusercontent.com";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: i64,
    pub token_type: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    token_type: String,
}

#[derive(Debug, Clone)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> PkceChallenge {
    let verifier: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();
    let challenge = URL_SAFE_NO_PAD.encode(hash);

    PkceChallenge { verifier, challenge }
}

pub fn token_path() -> Result<PathBuf> {
    crate::config::token_path()
}

pub fn load_stored_token() -> Result<Option<StoredToken>> {
    let path = token_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let data = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read token file at {:?}", path))?;
    let token: StoredToken = serde_json::from_str(&data)
        .with_context(|| format!("Failed to parse token JSON from {:?}", path))?;
    Ok(Some(token))
}

pub fn save_stored_token(token: &StoredToken) -> Result<()> {
    let path = token_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(token)?;
    fs::write(&path, data)
        .with_context(|| format!("Failed to write token to {:?}", path))?;
    Ok(())
}

pub struct TokenManager {
    token: StoredToken,
    client: reqwest::Client,
}

impl TokenManager {
    pub async fn load_or_init() -> Result<Self> {
        let stored = load_stored_token()?
            .context("No OAuth token found. Please run `gdsync auth` to authenticate.")?;
        Ok(Self {
            token: stored,
            client: reqwest::Client::new(),
        })
    }

    pub fn from_token(token: StoredToken) -> Self {
        Self {
            token,
            client: reqwest::Client::new(),
        }
    }

    pub async fn get_access_token(&mut self) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // If expires in less than 60 seconds, refresh it
        if self.token.expires_at - now < 60 {
            self.refresh_access_token().await?;
        }

        Ok(self.token.access_token.clone())
    }

    pub async fn refresh_access_token(&mut self) -> Result<()> {
        let refresh_token = match &self.token.refresh_token {
            Some(rt) => rt.clone(),
            None => bail!("Cannot refresh token: no refresh_token stored. Run `gdsync auth` again."),
        };

        debug!("Refreshing Google OAuth access token...");

        let mut params = vec![
            ("client_id", self.token.client_id.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ];

        let secret = self.token.client_secret.clone();
        if let Some(ref s) = secret {
            params.push(("client_secret", s.as_str()));
        }

        let resp = self
            .client
            .post(GOOGLE_TOKEN_ENDPOINT)
            .form(&params)
            .send()
            .await
            .context("Failed to send token refresh request")?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            bail!("Failed to refresh OAuth token: {}", err_text);
        }

        let token_resp: TokenResponse = resp
            .json()
            .await
            .context("Failed to parse refresh token response")?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        self.token.access_token = token_resp.access_token;
        self.token.expires_at = now + token_resp.expires_in;
        if let Some(new_rt) = token_resp.refresh_token {
            self.token.refresh_token = Some(new_rt);
        }

        save_stored_token(&self.token)?;
        debug!("OAuth token successfully refreshed and saved.");
        Ok(())
    }
}

/// Executes interactive OAuth2 PKCE login.
pub async fn execute_oauth_login(
    client_id: &str,
    client_secret: Option<String>,
) -> Result<StoredToken> {
    let pkce = generate_pkce();

    let auth_url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={}&code_challenge_method=S256&access_type=offline&prompt=consent",
        GOOGLE_AUTH_ENDPOINT,
        client_id,
        urlencoding::encode(REDIRECT_URI),
        urlencoding::encode(DRIVE_SCOPE),
        pkce.challenge
    );

    println!("\n=== Google Drive Authentication ===");
    println!("Opening your browser to complete Google Drive authentication...");
    println!("If the browser does not open automatically, open this URL:\n");
    println!("{}\n", auth_url);

    // Try to open browser with xdg-open on Linux
    let _ = std::process::Command::new("xdg-open")
        .arg(&auth_url)
        .spawn();

    // Start local TCP listener to capture redirect callback
    let listener = TcpListener::bind(format!("127.0.0.1:{}", REDIRECT_PORT))
        .with_context(|| format!("Failed to bind to 127.0.0.1:{}", REDIRECT_PORT))?;

    println!("Waiting for authentication callback on port {}...", REDIRECT_PORT);

    let (mut stream, _) = listener.accept().context("Failed to accept callback connection")?;
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    // Parse HTTP request line: GET /oauth2callback?code=... HTTP/1.1
    let auth_code = parse_code_from_request(&request_line)
        .context("Failed to parse authorization code from HTTP redirect")?;

    // Respond to user's browser
    let response_body = r#"<!DOCTYPE html>
<html>
<head><title>Authentication Successful</title></head>
<body style="font-family: sans-serif; text-align: center; padding-top: 50px;">
  <h1 style="color: #2e7d32;">Authentication Successful!</h1>
  <p>You can close this tab and return to your terminal.</p>
</body>
</html>"#;

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();

    println!("Received authorization code. Exchanging for access tokens...");

    // Exchange auth code for tokens
    let http_client = reqwest::Client::new();
    let mut params = vec![
        ("client_id", client_id),
        ("code", auth_code.as_str()),
        ("grant_type", "authorization_code"),
        ("redirect_uri", REDIRECT_URI),
        ("code_verifier", pkce.verifier.as_str()),
    ];

    if let Some(ref s) = client_secret {
        params.push(("client_secret", s.as_str()));
    }

    let resp = http_client
        .post(GOOGLE_TOKEN_ENDPOINT)
        .form(&params)
        .send()
        .await
        .context("Failed to exchange code for tokens")?;

    if !resp.status().is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        bail!("Google token exchange failed: {}", err_text);
    }

    let token_resp: TokenResponse = resp.json().await.context("Failed to parse token response")?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let stored = StoredToken {
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token,
        expires_at: now + token_resp.expires_in,
        token_type: token_resp.token_type,
        client_id: client_id.to_string(),
        client_secret,
    };

    save_stored_token(&stored)?;
    println!("Authentication succeeded! Tokens saved to {:?}", token_path()?);
    Ok(stored)
}

fn parse_code_from_request(request_line: &str) -> Option<String> {
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let url = parts[1];
    let query = url.split('?').nth(1)?;
    for pair in query.split('&') {
        let mut kv = pair.split('=');
        if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
            if k == "code" {
                return urlencoding::decode(v).ok();
            }
        }
    }
    None
}

// Minimal urlencoding helper
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut result = String::new();
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    result.push(byte as char);
                }
                _ => {
                    result.push_str(&format!("%{:02X}", byte));
                }
            }
        }
        result
    }

    pub fn decode(s: &str) -> Result<String, ()> {
        let mut result = Vec::new();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(val) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|_| ())?, 16) {
                    result.push(val);
                    i += 3;
                    continue;
                }
            } else if bytes[i] == b'+' {
                result.push(b' ');
                i += 1;
                continue;
            }
            result.push(bytes[i]);
            i += 1;
        }
        String::from_utf8(result).map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pkce_generation() {
        let pkce = generate_pkce();
        assert_eq!(pkce.verifier.len(), 64);
        assert!(!pkce.challenge.is_empty());
        assert!(!pkce.challenge.contains('+'));
        assert!(!pkce.challenge.contains('/'));
        assert!(!pkce.challenge.contains('='));
    }

    #[test]
    fn test_parse_code() {
        let req = "GET /oauth2callback?code=4%2F0AY0e-g7XYZ&scope=email HTTP/1.1\r\n";
        let code = parse_code_from_request(req);
        assert_eq!(code, Some("4/0AY0e-g7XYZ".to_string()));
    }
}
