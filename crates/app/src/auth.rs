//! Google OAuth 어댑터. 시스템 브라우저 + loopback redirect + PKCE 흐름을 사용한다.

use std::time::Duration;

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, RedirectUrl, RefreshToken, RequestTokenError, Scope, TokenResponse,
    TokenUrl,
};
use rust_i18n::t;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

use crate::token_store;

// 데스크톱 앱 유형 client의 secret은 Google 문서상 기밀로 취급되지 않으며 PKCE로 코드 탈취를 막는다.
const CLIENT_ID: &str = env!("GOOGLE_OAUTH_CLIENT_ID");
const CLIENT_SECRET: &str = env!("GOOGLE_OAUTH_CLIENT_SECRET");

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";

// 앱이 만들거나 사용자가 앱으로 연 파일로만 접근이 제한되는 최소 scope다.
// 다른 계정이 공유한 파일 접근 가능 여부는 아직 검증하지 않았다.
const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUEST_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleAccount {
    pub email: String,
}

/// Drive API 호출용 access token. 로그에 노출되지 않게 Debug를 가린다.
#[derive(Clone, PartialEq, Eq)]
pub struct DriveToken(String);

impl DriveToken {
    pub fn secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for DriveToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DriveToken(***)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub account: GoogleAccount,
    pub drive_token: DriveToken,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{}", t!("auth_error.denied", reason = .0))]
    Denied(String),
    #[error("{}", t!("auth_error.timed_out"))]
    TimedOut,
    #[error("{}", t!("auth_error.browser_unavailable", reason = .0))]
    BrowserUnavailable(#[source] std::io::Error),
    #[error("{}", t!("error.network", reason = .0))]
    Network(String),
    #[error("{}", t!("auth_error.drive_not_granted"))]
    DriveNotGranted,
    #[error("{}", t!("auth_error.rejected", reason = .0))]
    Rejected(String),
    #[error("{}", t!("error.internal", reason = .0))]
    Internal(String),
}

pub async fn sign_in() -> Result<Session, AuthError> {
    // IPv6 전용 환경을 고려할 필요가 없으므로 127.0.0.1에 임의 포트로 바인드한다.
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| {
        AuthError::Internal(t!("auth_error.loopback_bind", reason = e).into_owned())
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| AuthError::Internal(e.to_string()))?
        .port();

    let client = oauth_client()?
        .set_redirect_uri(RedirectUrl::new(format!("http://127.0.0.1:{port}")).map_err(internal)?);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("openid".to_owned()))
        .add_scope(Scope::new("email".to_owned()))
        .add_scope(Scope::new(DRIVE_SCOPE.to_owned()))
        // refresh token을 받아 다음 실행 때 로그인을 유지한다.
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "select_account consent")
        .set_pkce_challenge(pkce_challenge)
        .url();

    webbrowser::open(auth_url.as_str()).map_err(AuthError::BrowserUnavailable)?;

    let code = tokio::time::timeout(LOGIN_TIMEOUT, receive_code(&listener, csrf.secret()))
        .await
        .map_err(|_| AuthError::TimedOut)??;
    drop(listener);

    let http = http_client()?;
    let token = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http)
        .await
        .map_err(token_error)?;

    // 세분화된 동의 화면에서 사용자가 Drive 항목만 해제할 수 있다.
    // scope가 응답에 없으면 RFC 6749 5.1에 따라 요청한 scope 그대로 허용된 것이다.
    if let Some(scopes) = token.scopes()
        && !scopes.iter().any(|s| s.as_str() == DRIVE_SCOPE)
    {
        return Err(AuthError::DriveNotGranted);
    }

    let session = session_from(&http, token.access_token().secret()).await?;
    if let Some(refresh) = token.refresh_token() {
        // 저장에 실패해도 이번 로그인은 유효하다. 다음 실행 때 다시 로그인하면 된다.
        if let Err(e) = token_store::save(refresh.secret().clone()).await {
            dioxus::logger::tracing::warn!("refresh token 저장 실패: {e}");
        }
    }
    Ok(session)
}

/// 저장된 refresh token으로 로그인을 복원한다. 저장된 토큰이 없거나 폐기됐으면 `None`.
pub async fn restore() -> Result<Option<Session>, AuthError> {
    let Some(refresh) = token_store::load().await.map_err(internal)? else {
        return Ok(None);
    };
    let http = http_client()?;
    let result = oauth_client()?
        .exchange_refresh_token(&RefreshToken::new(refresh))
        .request_async(&http)
        .await;
    let token = match result {
        Ok(token) => token,
        // 사용자가 권한을 철회했거나 토큰이 만료됐다. 다시 로그인하게 한다.
        Err(RequestTokenError::ServerResponse(e)) if e.error().to_string() == "invalid_grant" => {
            token_store::clear().await.map_err(internal)?;
            return Ok(None);
        }
        Err(e) => return Err(token_error(e)),
    };
    session_from(&http, token.access_token().secret())
        .await
        .map(Some)
}

pub async fn sign_out() -> Result<(), AuthError> {
    token_store::clear().await.map_err(internal)
}

type OAuthClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

fn oauth_client() -> Result<OAuthClient, AuthError> {
    Ok(BasicClient::new(ClientId::new(CLIENT_ID.to_owned()))
        .set_client_secret(ClientSecret::new(CLIENT_SECRET.to_owned()))
        .set_auth_uri(AuthUrl::new(AUTH_URL.to_owned()).map_err(internal)?)
        .set_token_uri(TokenUrl::new(TOKEN_URL.to_owned()).map_err(internal)?))
}

fn http_client() -> Result<reqwest::Client, AuthError> {
    reqwest::Client::builder()
        // 토큰 엔드포인트의 redirect를 따라가면 SSRF 경로가 생기므로 막는다.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(internal)
}

fn token_error<RE: std::error::Error + 'static>(
    e: RequestTokenError<RE, oauth2::basic::BasicErrorResponse>,
) -> AuthError {
    match e {
        RequestTokenError::Request(e) => AuthError::Network(e.to_string()),
        RequestTokenError::ServerResponse(e) => AuthError::Rejected(e.error().to_string()),
        other => AuthError::Internal(other.to_string()),
    }
}

async fn session_from(http: &reqwest::Client, access_token: &str) -> Result<Session, AuthError> {
    let account = fetch_email(http, access_token).await?;
    // access token 만료(약 1시간) 시 자동 갱신은 아직 없다. 앱을 다시 시작하면 복원된다.
    Ok(Session {
        account,
        drive_token: DriveToken(access_token.to_owned()),
    })
}

async fn receive_code(listener: &TcpListener, expected_state: &str) -> Result<String, AuthError> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|e| {
            AuthError::Internal(t!("auth_error.loopback_accept", reason = e).into_owned())
        })?;

        let Some(target) = read_request_target(&mut stream).await else {
            continue;
        };
        let callback = parse_callback(&target);
        let (status, body) = match &callback {
            Callback::Ignored => ("404 Not Found", String::new()),
            Callback::Code { .. } | Callback::Error(_) => (
                "200 OK",
                format!(
                    "<html><body><p>{}</p></body></html>",
                    t!("login.callback_done")
                ),
            ),
        };
        respond(&mut stream, status, &body).await;

        match callback {
            Callback::Ignored => continue,
            Callback::Error(error) => return Err(AuthError::Denied(error)),
            Callback::Code { code, state } if state == expected_state => return Ok(code),
            // 다른 탭·이전 시도의 redirect일 수 있으므로 상태 불일치는 무시하고 계속 기다린다.
            Callback::Code { .. } => continue,
        }
    }
}

async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() >= MAX_REQUEST_BYTES {
            return None;
        }
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let line = std::str::from_utf8(&buf).ok()?.lines().next()?;
    let mut parts = line.split_whitespace();
    (parts.next()? == "GET").then_some(())?;
    parts.next().map(str::to_owned)
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // 브라우저 응답 실패는 로그인 결과에 영향을 주지 않는다.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[derive(Debug, PartialEq, Eq)]
enum Callback {
    Code { code: String, state: String },
    Error(String),
    Ignored,
}

fn parse_callback(target: &str) -> Callback {
    let Ok(url) = Url::parse("http://127.0.0.1").and_then(|base| base.join(target)) else {
        return Callback::Ignored;
    };
    if url.path() != "/" {
        return Callback::Ignored;
    }
    let (mut code, mut state, mut error) = (None, None, None);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    match (code, state, error) {
        (_, _, Some(error)) => Callback::Error(error),
        (Some(code), Some(state), None) => Callback::Code { code, state },
        _ => Callback::Ignored,
    }
}

#[derive(Deserialize)]
struct UserInfo {
    email: Option<String>,
    email_verified: Option<bool>,
}

async fn fetch_email(
    http: &reqwest::Client,
    access_token: &str,
) -> Result<GoogleAccount, AuthError> {
    let response = http
        .get(USERINFO_URL)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| AuthError::Network(e.without_url().to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(AuthError::Rejected(format!("userinfo HTTP {status}")));
    }
    let info: UserInfo = response.json().await.map_err(|e| {
        AuthError::Internal(t!("auth_error.userinfo_parse", reason = e).into_owned())
    })?;
    match (info.email, info.email_verified) {
        (Some(email), Some(true)) => Ok(GoogleAccount { email }),
        _ => Err(AuthError::Rejected(
            t!("auth_error.email_unverified").into_owned(),
        )),
    }
}

fn internal(e: impl std::fmt::Display) -> AuthError {
    AuthError::Internal(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_and_state() {
        assert_eq!(
            parse_callback("/?state=abc&code=4%2F0xyz&scope=email"),
            Callback::Code {
                code: "4/0xyz".to_owned(),
                state: "abc".to_owned()
            }
        );
    }

    #[test]
    fn error_takes_precedence() {
        assert_eq!(
            parse_callback("/?error=access_denied&state=abc"),
            Callback::Error("access_denied".to_owned())
        );
    }

    #[test]
    fn ignores_unrelated_requests() {
        assert_eq!(parse_callback("/favicon.ico"), Callback::Ignored);
        assert_eq!(parse_callback("/"), Callback::Ignored);
        assert_eq!(parse_callback("/?code=only"), Callback::Ignored);
    }

    #[test]
    fn embeds_client_credentials() {
        assert!(CLIENT_ID.ends_with(".apps.googleusercontent.com"));
        assert!(!CLIENT_SECRET.is_empty());
    }
}
