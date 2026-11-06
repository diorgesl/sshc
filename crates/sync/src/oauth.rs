//! Passos de OAuth 2.0 para apps desktop (PKCE, sem client secret).

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const SCOPE: &str = "https://www.googleapis.com/auth/drive.appdata";

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

pub fn auth_url(client_id: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}\
&code_challenge={}&code_challenge_method=S256&state={}&access_type=offline&prompt=consent",
        urlencode(client_id),
        urlencode(redirect_uri),
        urlencode(SCOPE),
        urlencode(challenge),
        urlencode(state),
    )
}

/// Extrai `code` e `state` da primeira linha do redirecionamento loopback.
pub fn parse_redirect(request: &str) -> Result<(String, String)> {
    let line = request.lines().next().unwrap_or_default();
    let target = line.split_whitespace().nth(1).unwrap_or_default();
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = percent_decode(v);
        match k {
            "code" => code = Some(v),
            "state" => state = Some(v),
            "error" => error = Some(v),
            _ => {}
        }
    }
    if let Some(err) = error {
        bail!("autorização negada pelo provedor: {err}");
    }
    let code = code.context("redirecionamento sem 'code'")?;
    let state = state.unwrap_or_default();
    Ok((code, state))
}

pub fn exchange_code(
    client: &reqwest::blocking::Client,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse> {
    post_token(
        client,
        &[
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ],
    )
}

pub fn refresh(
    client: &reqwest::blocking::Client,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    post_token(
        client,
        &[
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ],
    )
}

fn post_token(
    client: &reqwest::blocking::Client,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    let resp = client
        .post(TOKEN_URL)
        .form(form)
        .send()
        .context("chamando o endpoint de token")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("endpoint de token respondeu {status}: {body}");
    }
    resp.json::<TokenResponse>().context("resposta de token inválida")
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_is_parsed() {
        let req = "GET /?state=abc123&code=4%2F0AX4XfWg%2Fxyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        let (code, state) = parse_redirect(req).unwrap();
        assert_eq!(code, "4/0AX4XfWg/xyz");
        assert_eq!(state, "abc123");
    }

    #[test]
    fn redirect_error_is_reported() {
        let req = "GET /?error=access_denied HTTP/1.1\r\n\r\n";
        assert!(parse_redirect(req).is_err());
    }

    #[test]
    fn auth_url_encodes_scope_and_redirect() {
        let url = auth_url("cid", "http://127.0.0.1:1234", "chal", "st");
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1234"));
        assert!(url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.appdata"));
        assert!(url.contains("code_challenge_method=S256"));
    }
}
