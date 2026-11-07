use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "stirling",
    version,
    about = "Cross-platform development and operations CLI for Stirling PDF"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Emit machine-readable JSON instead of human output where supported.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Build the Rust workspace.
    Build {
        /// Build in release mode.
        #[arg(long)]
        release: bool,
    },
    /// Format check + clippy + tests (the `task check` equivalent for Rust).
    Check,
    /// Run clippy across the workspace.
    Lint,
    /// Auto-format Rust sources.
    Fmt,
    /// Remove build artifacts.
    Clean,
    /// Rebuild on change (requires cargo-watch).
    Watch,
    /// Run tests.
    Test {
        #[command(subcommand)]
        kind: Option<TestKind>,
    },
    /// Start a dev component detached, logging to .run/logs.
    Start {
        name: String,
        /// Working directory for the process (relative to repo root or absolute).
        #[arg(long)]
        cwd: Option<String>,
        /// Environment variable, KEY=VALUE. Repeatable.
        #[arg(long = "env")]
        env: Vec<String>,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Stop managed components; with no names, stops everything.
    Stop { names: Vec<String> },
    /// Restart components.
    Restart { names: Vec<String> },
    /// Show status of managed components.
    Status,
    /// Tail a component's log.
    Logs {
        name: String,
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
    /// HTTP health check against a running backend.
    Health {
        #[arg(long, default_value = "http://127.0.0.1:8081/health")]
        url: String,
    },
    /// Poll a URL until it returns 2xx or the timeout expires.
    Wait { seconds: u64, url: String },
    /// Find free TCP ports (replaces scripts/find-free-port.sh).
    #[command(subcommand)]
    Ports(PortsCmd),
    /// Check the development environment.
    Doctor,
    /// Write a diagnostic bundle to .run/diagnostics.
    Diagnose,
    /// Print a project/tooling status report.
    Report,
    /// Manage external PDF engines (ghostscript, qpdf, tesseract, ...).
    #[command(subcommand)]
    Engines(EnginesCmd),
    /// Database maintenance.
    #[command(subcommand)]
    Db(DbCmd),
    /// Configuration management.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Install the stirling binaries into the user bin directory.
    Install {
        /// Build release binaries first.
        #[arg(long)]
        release: bool,
    },
    /// Remove installed stirling binaries.
    Uninstall,
    /// Build release artifacts and report their locations.
    Package {
        /// Also produce a .tar.gz / .zip archive per target.
        #[arg(long)]
        archive: bool,
    },
    /// Manage the stirling service (systemd/launchd).
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Shell-script migration auditing.
    #[command(subcommand)]
    Migration(MigrationCmd),
}

#[derive(Subcommand)]
pub enum PortsCmd {
    /// Print one free port per preferred port: the preferred port if free,
    /// otherwise a distinct random free port in 20000-49999.
    Free { ports: Vec<u16> },
}

#[derive(Subcommand)]
pub enum TestKind {
    /// Unit tests in the stirling-server crate; optional name filter.
    Unit { filter: Option<String> },
    /// Workspace tests (all targets).
    Integration,
    /// Boot the server and run the multi-endpoint smoke script.
    Runtime,
    /// GeneralApi endpoint smoke (transitional: delegates to shell script).
    General,
    /// SecurityApi endpoint smoke (transitional: delegates to shell script).
    Security,
    /// Per-file test disposition report (coverage-report.sh equivalent).
    Coverage,
    /// Generate/check test fixtures.
    Fixtures,
    /// Conversion smoke coverage.
    Conversions,
    /// The stirling-engine crate test suite.
    Engine,
}

#[derive(Subcommand)]
pub enum EnginesCmd {
    /// List all known engines and whether they were found.
    List,
    /// Exit non-zero if a required engine is missing.
    Check,
    /// Show detected versions only.
    Versions,
    /// Probe each engine's --version and report parse status.
    Validate,
    /// Detailed machine-readable engine report.
    Report,
    /// Install a missing engine through the platform package manager.
    /// Always prints the command and asks before running it.
    Install {
        /// Engine name (e.g. ghostscript, tesseract, qpdf).
        engine: String,
        /// Run the package-manager command without prompting.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum DbCmd {
    Status,
    Migrate,
    Backup,
    Restore { file: String },
    Verify,
    Vacuum,
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Create configs/custom_settings.yml if absent.
    Init,
    /// Parse and validate settings files.
    Validate,
    /// Print the effective configuration.
    Show,
}

#[derive(Debug, Subcommand)]
pub enum ServiceCmd {
    Install,
    Remove,
    Enable,
    Disable,
    Status,
}

#[derive(Subcommand)]
pub enum MigrationCmd {
    /// Scan the repo for shell scripts and classify them.
    Audit,
    /// Audit plus Taskfile/CI reference counts.
    Report,
}
