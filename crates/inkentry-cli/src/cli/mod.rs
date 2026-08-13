use clap::{Parser, Subcommand};

pub mod cmd;
mod removed;

// Re-export top-level Args types so callers can use `crate::cli::XxxArgs`.
// Sub-command Args types (Memory*Args, Plumbing*Args, etc.) are accessed via
// their owning modules (e.g. `crate::cli::cmd::memory::MemoryAddArgs`) when needed.
pub use cmd::auth::AuthArgs;
pub use cmd::context::ContextArgs;
pub use cmd::harvest::HarvestArgs;
pub use cmd::hooks::HooksArgs;
pub use cmd::import::ImportArgs;
pub use cmd::index::IndexArgs;
pub use cmd::init::InitArgs;
pub use cmd::link::{LinkArgs, UnlinkArgs};
pub use cmd::links::LinksArgs;
pub use cmd::login::LoginArgs;
pub use cmd::logout::LogoutArgs;
pub use cmd::memory::MemoryArgs;
pub use cmd::memory::MemorySyncArgs as SyncArgs;
pub use cmd::misc::ChunksArgs;
pub use cmd::org::OrgArgs;
pub use cmd::plumbing::PlumbingArgs;
pub use cmd::search::SearchArgs;
pub use cmd::server::ServerArgs;
pub use cmd::status::StatusArgs;

/// inkentry — local code intelligence
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    long_about = None,
    before_help = concat!("inkentry v", env!("CARGO_PKG_VERSION"))
)]
pub struct Cli {
    /// Path to config file (default: ~/.config/inkentry/config.toml)
    #[arg(short, long, global = true)]
    pub config: Option<std::path::PathBuf>,

    /// Color output: auto (default, on when stdout is a terminal and
    /// NO_COLOR is unset), always, or never
    #[arg(long, global = true, value_enum, default_value_t = cmd::ColorChoice::Auto)]
    pub color: cmd::ColorChoice,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialise inkentry for the current project
    Init(InitArgs),
    /// Index a codebase directory
    Index(IndexArgs),
    /// Unified search over code and memory (interleaved, best-available ranking)
    Search(SearchArgs),
    /// Show index statistics (for current project or all registered projects)
    Status(StatusArgs),
    /// Print agent session context: handoffs, open questions, decisions, and requirements
    Context(ContextArgs),
    /// List supported languages
    Languages,
    /// Show the raw indexed chunks for a file (useful for debugging/agent use)
    Chunks(ChunksArgs),
    /// Add a dependency: current project also searches another project's index
    Link(LinkArgs),
    /// Remove a previously added dependency
    Unlink(UnlinkArgs),
    /// Remove registry entries for projects whose root path no longer exists
    Autoclean,
    /// Project memory: store and query decisions, context, and requirements
    Memory(MemoryArgs),
    /// Capture memory from git history and session logs (backfill + continuous)
    Harvest(HarvestArgs),
    /// Import a portable dump into this project (see docs/dump-format.md)
    Import(ImportArgs),
    /// Manage git hooks (post-commit auto-index and harvest)
    Hooks(HooksArgs),
    /// Manage and inspect cross-project links
    Links(LinksArgs),
    /// Low-level plumbing commands for agents and scripts (JSONL output)
    Plumbing(PlumbingArgs),
    /// Two-way sync of local memory with the configured server (shorthand for `memory sync`)
    Sync(SyncArgs),
    /// Manage the local inkentry-server daemon (start / stop / status / logs)
    Server(ServerArgs),
    /// Authenticate with inkentry cloud (WorkOS device authorization)
    Login(LoginArgs),
    /// Remove stored inkentry cloud credentials (see `--servers`/`--server` for self-hosted keys)
    Logout(LogoutArgs),
    /// Manage the active organization (e.g. `inkentry org switch <org>`)
    Org(OrgArgs),
    /// Manage per-server bearer credentials (`set-key`, `list-servers`)
    Auth(AuthArgs),
}

impl Cli {
    /// Parse argv, substituting a migration hint for clap's error when the
    /// invocation names a surface this release removed (ADR-082). Diverges on
    /// any parse failure, exactly as `Cli::parse` does, so `--help` and
    /// `--version` keep clap's own exit path.
    pub fn parse_or_exit() -> Self {
        match Self::try_parse() {
            Ok(cli) => cli,
            Err(err) => {
                let argv: Vec<String> = std::env::args().collect();
                match removed::hint(&argv) {
                    Some(msg) => {
                        eprintln!("{msg}");
                        std::process::exit(2);
                    }
                    None => err.exit(),
                }
            }
        }
    }
}
