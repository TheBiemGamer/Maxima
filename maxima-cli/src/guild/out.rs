//! JSON output on stdout and scrubbed logging on stderr for the `guild` commands.

use std::{
    borrow::Cow,
    env,
    io::{self, Write},
};

use lazy_static::lazy_static;
use log::{Level, LevelFilter, Metadata, Record};
use regex::{Captures, Regex};
use serde::Serialize;

#[allow(dead_code)] // variants come into use as each command is implemented
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    LoginRequired,
    Login,
    NotOwned,
    NoBuild,
    Path,
    UnsafePath,
    Download,
    Touchup,
    ElevationDeclined,
    PortInUse,
    Launch,
    Internal,
}

#[derive(Serialize)]
struct ErrorEvent {
    error: ErrorCode,
}

/// Writes one JSON object as a single line on stdout and flushes it.
pub fn emit<T: Serialize>(event: &T) {
    let line = match serde_json::to_string(event) {
        Ok(line) => line,
        Err(err) => {
            log::error!("Failed to serialize event: {}", err);
            r#"{"error":"internal"}"#.to_owned()
        }
    };

    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{}", line);
    let _ = stdout.flush();
}

/// Emits `{"error":code}` and returns the exit code for a failure.
pub fn fail(code: ErrorCode) -> i32 {
    emit(&ErrorEvent { error: code });
    1
}

lazy_static! {
    static ref SIGNED_URL: Regex =
        Regex::new(r#"(https?://[^\s?"'<>`]+)\?([^\s"'<>`]*)"#).unwrap();
    static ref SIGNED_QUERY_KEY: Regex =
        Regex::new(r"(?i)(^|[&;])(sig|token|signature|expires)=").unwrap();
    static ref SECRET_QUERY: Regex = Regex::new(
        r#"(^|[?&#;\s])(access_token|refresh_token|code|pc_sign|code_challenge|ea_eadmtoken|ea_password|token)=[^&#\s"'<>`]*"#
    )
    .unwrap();
    static ref SECRET_JSON: Regex = Regex::new(
        r#""(access_token|refresh_token|id_token|code|pc_sign|code_challenge)"\s*:\s*"[^"]*""#
    )
    .unwrap();
    static ref BACKTICK_TOKEN: Regex = Regex::new(r"(?i)(token\s*)`[^`]*`").unwrap();
    static ref JWT: Regex = Regex::new(r"eyJ[\w-]+\.[\w-]+\.[\w-]+").unwrap();
}

/// Removes tokens, auth codes, PKCE challenges, request signatures and signed URL queries.
pub fn scrub(text: &str) -> String {
    let text = SIGNED_URL.replace_all(text, |caps: &Captures| {
        if SIGNED_QUERY_KEY.is_match(&caps[2]) {
            format!("{}?***", &caps[1])
        } else {
            caps[0].to_owned()
        }
    });
    let text = SECRET_QUERY.replace_all(&text, "$1$2=***");
    let text = SECRET_JSON.replace_all(&text, "\"$1\":\"***\"");
    let text = BACKTICK_TOKEN.replace_all(&text, "$1`***`");
    let text: Cow<str> = JWT.replace_all(&text, "***");
    text.into_owned()
}

struct StderrLogger;
static STDERR_LOGGER: StderrLogger = StderrLogger;

fn max_level() -> Level {
    match env::var("MAXIMA_LOG_LEVEL") {
        Ok(level) if level == "debug" => Level::Debug,
        _ => Level::Info,
    }
}

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let message = scrub(&record.args().to_string());
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "{} {} - {}",
            record.level(),
            record.target(),
            message
        );
    }

    fn flush(&self) {
        let _ = io::stderr().flush();
    }
}

/// Installs the stderr logger. Call before `maxima::util::log::init_logger`,
/// which then fails to install its stdout logger and does nothing.
pub fn init_stderr_logger() {
    if log::set_logger(&STDERR_LOGGER).is_ok() {
        log::set_max_level(LevelFilter::Trace);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_hides_query_secrets() {
        let out = scrub("https://accounts.ea.com/connect/auth?client_id=JUNO&access_token=eyJabc.def&code=QUOx&pc_sign=xyz&code_challenge=c1&nonce=5");
        for kept in [
            "client_id=JUNO",
            "nonce=5",
            "access_token=***",
            "code=***",
            "pc_sign=***",
            "code_challenge=***",
        ] {
            assert!(out.contains(kept), "{out} should contain {kept}");
        }
        for gone in ["eyJabc", "QUOx", "xyz"] {
            assert!(!out.contains(gone), "{out} should not contain {gone}");
        }
    }

    #[test]
    fn scrub_hides_refresh_token_error() {
        let out = scrub("token `abcDEF123` could not be refreshed: 400");
        assert!(!out.contains("abcDEF123"), "{out}");
        assert!(out.contains("could not be refreshed: 400"), "{out}");
    }

    #[test]
    fn scrub_hides_bare_jwt() {
        let out = scrub("got eyJhbGciOiJSUzI1NiJ9.eyJpc3MiOiJlYSJ9.c2ln from server");
        assert_eq!(out, "got *** from server");
    }

    #[test]
    fn scrub_hides_signed_urls() {
        for url in [
            "https://cdn.ea.com/a/b.zip?sig=abc&x=1",
            "https://cdn.ea.com/a/b.zip?x=1&token=abc",
            "https://cdn.ea.com/a/b.zip?Signature=abc&Key-Pair-Id=k",
            "https://cdn.ea.com/a/b.zip?Expires=123&Policy=p",
        ] {
            let out = scrub(&format!("downloading {url} now"));
            assert_eq!(out, "downloading https://cdn.ea.com/a/b.zip?*** now", "{url}");
        }
        let plain = "see https://example.com/page?lang=en";
        assert_eq!(scrub(plain), plain);
    }

    #[test]
    fn scrub_hides_json_tokens() {
        let out = scrub(r#"{"access_token":"abc","refresh_token": "def","expires_in":3600}"#);
        assert!(!out.contains("abc") && !out.contains("def"), "{out}");
        assert!(out.contains(r#""expires_in":3600"#), "{out}");
    }

    #[test]
    fn error_code_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::ElevationDeclined).unwrap(),
            "\"elevation_declined\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::LoginRequired).unwrap(),
            "\"login_required\""
        );
    }
}
