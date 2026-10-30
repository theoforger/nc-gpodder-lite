use std::io::{self, BufRead, IsTerminal};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use nc_gpodder_lite::db::Db;
use nc_gpodder_lite::{AppState, Config, app, users};

/// Standalone sync server compatible with the nextcloud-gpodder (gpoddersync) Nextcloud app.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to the SQLite database file.
    #[arg(
        long,
        env = "NCGL_DB",
        default_value = "nc-gpodder-lite.db",
        global = true
    )]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server.
    Serve {
        /// Address to listen on.
        #[arg(long, env = "NCGL_LISTEN", default_value = "0.0.0.0:8080")]
        listen: SocketAddr,
        /// Public base URL, e.g. https://pods.example.com. Used in Login Flow v2
        /// responses. If unset, it's derived from Host / X-Forwarded-* headers.
        #[arg(long, env = "NCGL_PUBLIC_URL")]
        public_url: Option<String>,
        /// Allow fetching podcast feeds and images from loopback/private addresses.
        #[arg(long, env = "NCGL_ALLOW_LOCAL_REMOTE_SERVERS")]
        allow_local_remote_servers: bool,
    },
    /// Manage user accounts.
    #[command(subcommand)]
    User(UserCommand),
}

#[derive(Subcommand)]
enum UserCommand {
    /// Create a user. Prompts for the password (or reads it from stdin when piped).
    Add { username: String },
    /// Change a user's password.
    Passwd { username: String },
    /// Delete a user and all of their sync data.
    Delete { username: String },
    /// List users.
    List,
}

fn read_password() -> io::Result<String> {
    let password = if io::stdin().is_terminal() {
        let first = rpassword::prompt_password("Password: ")?;
        let second = rpassword::prompt_password("Repeat password: ")?;
        if first != second {
            return Err(io::Error::other("passwords do not match"));
        }
        first
    } else {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    };
    if password.is_empty() {
        return Err(io::Error::other("password must not be empty"));
    }
    Ok(password)
}

fn run_user_command(db: &Db, command: UserCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        UserCommand::Add { username } => {
            if username.is_empty() || username.contains(':') {
                return Err("username must be non-empty and must not contain ':'".into());
            }
            if db.with(|conn| users::user_id(conn, &username))?.is_some() {
                return Err(format!("user '{username}' already exists").into());
            }
            let password = read_password()?;
            db.with(|conn| users::create_user(conn, &username, &password))?;
            println!("Created user '{username}'.");
        }
        UserCommand::Passwd { username } => {
            if db.with(|conn| users::user_id(conn, &username))?.is_none() {
                return Err(format!("user '{username}' does not exist").into());
            }
            let password = read_password()?;
            db.with(|conn| users::set_password(conn, &username, &password))?;
            println!("Password changed for '{username}'.");
        }
        UserCommand::Delete { username } => {
            if !db.with(|conn| users::delete_user(conn, &username))? {
                return Err(format!("user '{username}' does not exist").into());
            }
            println!("Deleted user '{username}' and their sync data.");
        }
        UserCommand::List => {
            for username in db.with(|conn| users::list_users(conn))? {
                println!("{username}");
            }
        }
    }
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM (sent by `docker stop`).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nc_gpodder_lite=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let db = match Db::open(&cli.db) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("error: cannot open database {}: {err}", cli.db.display());
            return ExitCode::FAILURE;
        }
    };

    match cli.command {
        Command::User(command) => match run_user_command(&db, command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Serve {
            listen,
            public_url,
            allow_local_remote_servers,
        } => {
            let config = Config {
                public_url,
                allow_local_remote_servers,
            };
            let state = Arc::new(AppState::new(db, config));
            let listener = match tokio::net::TcpListener::bind(listen).await {
                Ok(listener) => listener,
                Err(err) => {
                    eprintln!("error: cannot listen on {listen}: {err}");
                    return ExitCode::FAILURE;
                }
            };
            tracing::info!("listening on http://{listen}");
            if let Err(err) = axum::serve(listener, app(state))
                .with_graceful_shutdown(shutdown_signal())
                .await
            {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
    }
}
