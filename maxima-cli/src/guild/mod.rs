//! `guild` commands: made for a program to drive. JSON lines on stdout, scrubbed
//! logs on stderr, exit code 0 only on success, and no native setup (no service,
//! no registry keys, no protocol handler, no browser).

mod login;
pub mod out;

use clap::Subcommand;
use log::error;
use maxima::core::LockedMaxima;

use out::{fail, scrub, ErrorCode};

#[derive(Subcommand, Debug, Clone)]
#[command(rename_all = "kebab-case")]
pub enum GuildCommand {
    /// Print the EA login URL, wait for the code and store the account
    Login,
    /// Remove the stored account
    Logout,
    /// Show who is logged in (name and user id only)
    Account,
    /// List owned full games
    Library,
    /// Show the live build and its sizes
    Info { offer: String },
    /// Download and install a game into an absolute path
    Install { offer: String, path: String },
    /// Register an existing install
    Import { offer: String, path: String },
    /// Delete a game's install folder
    Uninstall { offer: String },
    /// Start a game and stay alive until it stops
    Launch {
        offer: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
}

/// Runs one `guild` command and returns the process exit code.
pub async fn run(cmd: GuildCommand) -> i32 {
    install_panic_hook();

    match cmd {
        GuildCommand::Login => login::login().await,
        GuildCommand::Logout => login::logout().await,
        GuildCommand::Account => login::account().await,
        GuildCommand::Library
        | GuildCommand::Info { .. }
        | GuildCommand::Install { .. }
        | GuildCommand::Import { .. }
        | GuildCommand::Uninstall { .. }
        | GuildCommand::Launch { .. } => match logged_in_maxima().await {
            Ok(_) => fail(ErrorCode::Internal),
            Err(code) => code,
        },
    }
}

/// Maxima with a valid stored login, for every command except `login` and `account`.
/// Without one, prints `{"error":"login_required"}` and returns the exit code; never
/// opens a browser or waits for input.
pub(crate) async fn logged_in_maxima() -> Result<LockedMaxima, i32> {
    let maxima = login::load_maxima().await?;
    match login::is_logged_in(&maxima).await {
        Ok(true) => Ok(maxima),
        Ok(false) => Err(fail(ErrorCode::LoginRequired)),
        Err(err) => {
            error!("Failed to check the stored login: {}", err);
            Err(fail(ErrorCode::LoginRequired))
        }
    }
}

/// A panic anywhere ends the process with a scrubbed message on stderr and
/// `{"error":"internal"}` on stdout, so the caller never sees a half-finished run.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("ERROR panic - {}", scrub(&info.to_string()));
        std::process::exit(fail(ErrorCode::Internal));
    }));
}
