mod cli;
mod commands;
mod output;
mod platform;
mod services;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands, PortsCmd};
use services::filesystem;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let root = filesystem::repo_root(&std::env::current_dir()?);
    let json = cli.json;

    match cli.command {
        Commands::Build { release } => commands::dev::build(&root, release),
        Commands::Check => commands::dev::check(&root),
        Commands::Lint => commands::dev::lint(&root),
        Commands::Fmt => commands::dev::fmt(&root),
        Commands::Clean => commands::dev::clean(&root),
        Commands::Watch => commands::dev::watch(&root),
        Commands::Test { kind } => commands::test::run(&root, kind.as_ref()),
        Commands::Start {
            name,
            cwd,
            env,
            command,
        } => commands::runtime::start(&root, &name, &command, cwd.as_deref(), &env),
        Commands::Stop { names } => commands::runtime::stop(&root, &names),
        Commands::Restart { names } => commands::runtime::restart(&root, &names),
        Commands::Status => commands::runtime::status(&root),
        Commands::Logs { name, lines } => commands::runtime::logs(&root, &name, lines),
        Commands::Health { url } => commands::runtime::health(&url),
        Commands::Wait { seconds, url } => commands::runtime::wait(seconds, &url),
        Commands::Ports(PortsCmd::Free { ports }) => commands::runtime::ports_free(&ports),
        Commands::Doctor => commands::diagnostics::doctor(&root, json),
        Commands::Diagnose => commands::diagnostics::diagnose(&root),
        Commands::Report => commands::diagnostics::report(&root, json),
        Commands::Engines(cmd) => commands::engines::run(&root, &cmd, json),
        Commands::Db(cmd) => commands::database::run(&root, &cmd, json),
        Commands::Config(cmd) => commands::config::run(&root, &cmd, json),
        Commands::Install { release } => commands::install::install(&root, release),
        Commands::Uninstall => commands::install::uninstall(),
        Commands::Package { archive } => commands::install::package(&root, archive),
        Commands::Service(cmd) => commands::install::service(&root, &cmd),
        Commands::Migration(cmd) => commands::migration::run(&root, &cmd, json),
    }
}
