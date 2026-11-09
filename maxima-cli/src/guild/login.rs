//! `guild login`, `guild logout` and `guild account`.

use std::time::Duration;

use log::error;
use maxima::core::{
    auth::{
        context::AuthContext,
        login::{bind_oauth_listener, wait_for_oauth_code_on, OAUTH_LISTEN_ADDR},
        nucleus_token_exchange,
        storage::{AuthError, AuthStorage},
    },
    clients::JUNO_PC_CLIENT_ID,
    LockedMaxima, Maxima, MaximaOptionsBuilder,
};
use serde::{ser::SerializeMap, Serialize, Serializer};

use super::out::{emit, fail, ErrorCode};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum LoginEvent {
    /// Unscrubbed on purpose: the caller opens it. It carries `pc_sign` and the PKCE
    /// challenge, both single-use; it is never logged.
    LoginUrl(String),
    LoggedIn { name: String, user_id: String },
}

#[derive(Debug)]
pub enum AccountEvent {
    LoggedIn { name: String, user_id: String },
    LoggedOut,
}

impl Serialize for AccountEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            AccountEvent::LoggedIn { name, user_id } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("logged_in", &true)?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("user_id", user_id)?;
                map.end()
            }
            AccountEvent::LoggedOut => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("logged_in", &false)?;
                map.end()
            }
        }
    }
}

#[derive(Serialize)]
struct OkEvent {
    ok: bool,
}

/// Builds Maxima with the stored logins. No network calls yet.
pub(super) async fn load_maxima() -> Result<LockedMaxima, i32> {
    let options = MaximaOptionsBuilder::default()
        .load_auth_storage(true)
        .dummy_local_user(false)
        .build()
        .map_err(|err| {
            error!("Failed to configure Maxima: {}", err);
            fail(ErrorCode::Internal)
        })?;

    Maxima::new_with_options(options).await.map_err(|err| {
        error!("Failed to start Maxima: {}", err);
        fail(ErrorCode::Internal)
    })
}

/// Whether the selected account is valid, refreshing it if needed. Saves a refreshed token.
pub(super) async fn is_logged_in(maxima: &LockedMaxima) -> Result<bool, AuthError> {
    let maxima = maxima.lock().await;
    let mut storage = maxima.auth_storage().lock().await;
    if !storage.logged_in().await? {
        return Ok(false);
    }

    // `logged_in` may have refreshed the token without saving it; this saves it.
    storage.access_token().await?;
    Ok(true)
}

/// Display name and user id of the logged-in user.
async fn current_user(maxima: &LockedMaxima) -> Option<(String, String)> {
    let maxima = maxima.lock().await;
    match maxima.local_user().await {
        Ok(user) => {
            let name = user
                .player()
                .as_ref()
                .map(|player| player.display_name().to_owned())
                .unwrap_or_default();
            Some((name, user.id().to_owned()))
        }
        Err(err) => {
            error!("Failed to fetch the logged-in user: {}", err);
            None
        }
    }
}

pub async fn login() -> i32 {
    let maxima = match load_maxima().await {
        Ok(maxima) => maxima,
        Err(code) => return code,
    };

    let mut context = match AuthContext::new() {
        Ok(context) => context,
        Err(err) => {
            error!("Failed to prepare login: {}", err);
            return fail(ErrorCode::Login);
        }
    };

    let url = match context.nucleus_auth_url(JUNO_PC_CLIENT_ID, "code") {
        Ok(url) => url,
        Err(err) => {
            error!("Failed to build the login URL: {}", err);
            return fail(ErrorCode::Login);
        }
    };

    // Listen before printing the URL, so the caller can't send the code too early.
    let listener = match bind_oauth_listener().await {
        Ok(listener) => listener,
        Err(err) => {
            error!("Failed to listen on {}: {}", OAUTH_LISTEN_ADDR, err);
            return fail(ErrorCode::Login);
        }
    };

    emit(&LoginEvent::LoginUrl(url));

    match tokio::time::timeout(LOGIN_TIMEOUT, wait_for_oauth_code_on(&listener, &mut context))
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            error!("Login failed: {}", err);
            return fail(ErrorCode::Login);
        }
        Err(_) => {
            error!("Login timed out after {} seconds", LOGIN_TIMEOUT.as_secs());
            return fail(ErrorCode::Login);
        }
    }
    drop(listener);

    let token_res = match nucleus_token_exchange(&context).await {
        Ok(token_res) => token_res,
        Err(err) => {
            error!("Token exchange failed: {}", err);
            return fail(ErrorCode::Login);
        }
    };

    {
        let maxima = maxima.lock().await;
        let mut storage = maxima.auth_storage().lock().await;
        if let Err(err) = storage.add_account(&token_res).await {
            error!("Failed to store the account: {}", err);
            return fail(ErrorCode::Login);
        }
    }

    match current_user(&maxima).await {
        Some((name, user_id)) => {
            emit(&LoginEvent::LoggedIn { name, user_id });
            0
        }
        None => fail(ErrorCode::Login),
    }
}

/// Removes the selected account from auth storage. No network calls.
pub async fn logout() -> i32 {
    let storage = match AuthStorage::load() {
        Ok(storage) => storage,
        Err(err) => {
            error!("Failed to load auth storage: {}", err);
            return fail(ErrorCode::Internal);
        }
    };

    let result = storage.lock().await.remove_selected();
    match result {
        Ok(_) => {
            emit(&OkEvent { ok: true });
            0
        }
        Err(err) => {
            error!("Failed to save auth storage: {}", err);
            fail(ErrorCode::Internal)
        }
    }
}

pub async fn account() -> i32 {
    let maxima = match load_maxima().await {
        Ok(maxima) => maxima,
        Err(code) => return code,
    };

    match is_logged_in(&maxima).await {
        Ok(true) => {}
        Ok(false) => {
            emit(&AccountEvent::LoggedOut);
            return 0;
        }
        Err(err) => {
            error!("Failed to check the stored login: {}", err);
            emit(&AccountEvent::LoggedOut);
            return 0;
        }
    }

    match current_user(&maxima).await {
        Some((name, user_id)) => {
            emit(&AccountEvent::LoggedIn { name, user_id });
            0
        }
        None => fail(ErrorCode::Login),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_events_shape() {
        assert_eq!(
            serde_json::to_string(&LoginEvent::LoggedIn {
                name: "A".into(),
                user_id: "1".into()
            })
            .unwrap(),
            r#"{"logged_in":{"name":"A","user_id":"1"}}"#
        );
        assert_eq!(
            serde_json::to_string(&LoginEvent::LoginUrl("https://x/?a=1".into())).unwrap(),
            r#"{"login_url":"https://x/?a=1"}"#
        );
        assert_eq!(
            serde_json::to_string(&AccountEvent::LoggedOut).unwrap(),
            r#"{"logged_in":false}"#
        );
        assert_eq!(
            serde_json::to_string(&AccountEvent::LoggedIn {
                name: "A".into(),
                user_id: "1".into()
            })
            .unwrap(),
            r#"{"logged_in":true,"name":"A","user_id":"1"}"#
        );
    }
}
