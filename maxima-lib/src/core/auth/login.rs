use lazy_static::lazy_static;
use regex::Regex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::core::{auth::storage::AuthError, clients::JUNO_PC_CLIENT_ID};

use super::context::AuthContext;

lazy_static! {
    static ref HTTP_PATTERN: Regex =
        Regex::new(r"^([A-Za-z]+) +(.*) +(HTTP/[0-9][.][0-9])").unwrap();
}

pub async fn begin_oauth_login_flow<'a>(context: &mut AuthContext<'a>) -> Result<(), AuthError> {
    open::that(context.nucleus_auth_url(JUNO_PC_CLIENT_ID, "code")?)?;
    wait_for_oauth_code(context).await
}

/// Address the login redirect is forwarded to (by maxima-bootstrap or a program driving Maxima).
pub const OAUTH_LISTEN_ADDR: &str = "127.0.0.1:31033";

/// Binds the listener that receives the login redirect.
pub async fn bind_oauth_listener() -> Result<TcpListener, AuthError> {
    Ok(TcpListener::bind(OAUTH_LISTEN_ADDR).await?)
}

/// Binds `127.0.0.1:31033` and waits for `GET /auth?...code=...`, then stores the code in `context`.
pub async fn wait_for_oauth_code(context: &mut AuthContext<'_>) -> Result<(), AuthError> {
    let listener = bind_oauth_listener().await?;
    wait_for_oauth_code_on(&listener, context).await
}

const CLOSE_PAGE: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Maxima</title></head><body><p>You can close this window.</p></body></html>";
const NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Waits on an already bound listener for `GET /auth?...code=...` and stores the code in
/// `context`. Other requests are ignored. The request is answered with a short page.
pub async fn wait_for_oauth_code_on(
    listener: &TcpListener,
    context: &mut AuthContext<'_>,
) -> Result<(), AuthError> {
    loop {
        let (mut socket, _) = listener.accept().await?;

        let (read, mut write) = socket.split();
        let mut reader = BufReader::new(read);

        let mut line = String::new();
        if reader.read_line(&mut line).await.is_err() {
            continue;
        }

        let captures = match HTTP_PATTERN.captures(&line) {
            Some(cap) => cap,
            None => continue,
        };

        let path_and_query = captures.get(2).ok_or(AuthError::Query)?.as_str();
        if !path_and_query.starts_with("/auth") {
            let _ = write
                .write_all(NOT_FOUND)
                .await;
            continue;
        }

        let code = path_and_query
            .split_once("?")
            .map(|(_, qs)| qs.trim())
            .map(querystring::querify)
            .and_then(|query| {
                query
                    .into_iter()
                    .find(|(key, value)| *key == "code" && !value.is_empty())
                    .map(|(_, value)| value.to_owned())
            });

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            CLOSE_PAGE.len(),
            CLOSE_PAGE
        );
        let _ = write.write_all(response.as_bytes()).await;
        let _ = write.flush().await;

        return match code {
            Some(code) => {
                context.set_code(&code);
                Ok(())
            }
            None => Err(AuthError::NoAuthCode),
        };
    }
}

// Use the OOA API to retrieve an access token without a captcha
#[deprecated(note = "This method of login was patched and this function will be removed soon")]
pub async fn manual_login(_persona: &str, _password: &str) -> Result<String, AuthError> {
    unimplemented!();
}
