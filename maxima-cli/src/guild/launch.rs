//! `guild launch <offer> [-- args]`: license, cloud read sync, start the game,
//! stay alive while it runs (the LSX server answers its license and login
//! requests), cloud write sync after it stops.

use std::{env, net::TcpListener, time::Duration};

use maxima::core::launch::{self, LaunchError, LaunchMode, LaunchOptions};
use serde::Serialize;

use super::out::{emit, fail, scrub, ErrorCode};

const DEFAULT_LSX_PORT: u16 = 3216;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Serialize)]
struct StartedEvent {
    started: bool,
}

#[derive(Serialize)]
struct StoppedEvent {
    stopped: bool,
}

pub async fn run(offer_id: &str, args: Vec<String>) -> i32 {
    // Before anything else: `start_lsx` only logs a bind failure from its own
    // task, so a port held by the EA app (or another Maxima) would give a game
    // that can't get its license.
    let port = match lsx_port() {
        Some(port) => port,
        None => {
            log::error!("MAXIMA_LSX_PORT is not a valid port");
            return fail(ErrorCode::Internal);
        }
    };
    if !port_free(port) {
        log::error!("LSX port {} is already in use", port);
        return fail(ErrorCode::PortInUse);
    }

    let maxima_arc = match super::logged_in_maxima().await {
        Ok(maxima) => maxima,
        Err(code) => return code,
    };

    {
        let mut maxima = maxima_arc.lock().await;
        if let Err(err) = maxima.start_lsx(maxima_arc.clone()).await {
            log::error!(
                "Failed to start the LSX server: {}",
                scrub(&err.to_string())
            );
            return fail(ErrorCode::Launch);
        }
        // RTM only carries presence; the game runs without it.
        if let Err(err) = maxima.rtm().login().await {
            log::warn!("Failed to log in to RTM: {}", scrub(&err.to_string()));
        }
    }

    let options = LaunchOptions {
        path_override: None,
        arguments: args,
        cloud_saves: true,
    };
    if let Err(err) = launch::start_game(
        maxima_arc.clone(),
        LaunchMode::Online(offer_id.to_owned()),
        options,
    )
    .await
    {
        log::error!("Failed to launch {}: {}", offer_id, scrub(&err.to_string()));
        return fail(launch_error_code(&err));
    }

    emit(&StartedEvent { started: true });

    // The LSX handlers lock the same mutex, so it is released between polls.
    loop {
        {
            let mut maxima = maxima_arc.lock().await;
            maxima.update().await;
            maxima.consume_pending_events();
            if maxima.playing().is_none() {
                break;
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    emit(&StoppedEvent { stopped: true });
    0
}

fn launch_error_code(err: &LaunchError) -> ErrorCode {
    match err {
        LaunchError::NotInstalled(_) => ErrorCode::Path,
        LaunchError::NoOfferFound(_) => ErrorCode::NotOwned,
        _ => ErrorCode::Launch,
    }
}

/// The LSX port Maxima will use: `MAXIMA_LSX_PORT`, else 3216. `None` when the
/// variable is set but not a port.
fn lsx_port() -> Option<u16> {
    match env::var("MAXIMA_LSX_PORT") {
        Ok(port) => port.parse().ok(),
        Err(_) => Some(DEFAULT_LSX_PORT),
    }
}

/// Whether `127.0.0.1:port` can be bound right now.
pub fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_check_detects_busy_port() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_free(port));
        drop(listener);
        assert!(port_free(port));
    }

    #[test]
    fn launch_errors_map_to_codes() {
        assert_eq!(
            launch_error_code(&LaunchError::NotInstalled("x".into())),
            ErrorCode::Path
        );
        assert_eq!(
            launch_error_code(&LaunchError::NoOfferFound("x".into())),
            ErrorCode::NotOwned
        );
        assert_eq!(
            launch_error_code(&LaunchError::BootstrapMissing),
            ErrorCode::Launch
        );
        let spawn = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        assert_eq!(
            launch_error_code(&LaunchError::Spawn(spawn)),
            ErrorCode::Launch
        );
    }

    #[test]
    fn started_and_stopped_shape() {
        assert_eq!(
            serde_json::to_string(&StartedEvent { started: true }).unwrap(),
            r#"{"started":true}"#
        );
        assert_eq!(
            serde_json::to_string(&StoppedEvent { stopped: true }).unwrap(),
            r#"{"stopped":true}"#
        );
    }
}
