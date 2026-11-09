//! `guild` commands: made for a program to drive. JSON lines on stdout, scrubbed
//! logs on stderr, exit code 0 only on success, and no native setup (no service,
//! no registry keys, no protocol handler, no browser).

pub mod out;

use clap::Subcommand;

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
        GuildCommand::Login
        | GuildCommand::Logout
        | GuildCommand::Account
        | GuildCommand::Library
        | GuildCommand::Info { .. }
        | GuildCommand::Install { .. }
        | GuildCommand::Import { .. }
        | GuildCommand::Uninstall { .. }
        | GuildCommand::Launch { .. } => fail(ErrorCode::Internal),
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
