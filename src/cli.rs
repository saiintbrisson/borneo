use std::path::PathBuf;

use crate::manifest::Packaging;

#[derive(clap::ValueEnum, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

#[derive(clap::Parser)]
#[command(version, about = "A build tool for Java projects")]
pub struct Cli {
    #[arg(long, value_enum, global = true, default_value = "text")]
    pub format: OutputFormat,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(clap::Subcommand)]
pub enum Commands {
    /// Compile and package the project
    #[command(alias = "b")]
    Build(BuildCommand),
    /// Build and run the project
    #[command(alias = "r")]
    Run(RunCommand),
    /// Run tests
    #[command(alias = "t")]
    Test(TestCommand),
    /// Resolve dependencies and update the lock file
    #[command(alias = "s")]
    Sync(SyncCommand),
    /// Remove build artifacts and purge stale libraries
    Clean(CleanCommand),
}

#[derive(clap::Args)]
pub struct ProjectArgs {
    /// Base directory used to calculate all other paths.
    #[arg(long)]
    pub base: Option<PathBuf>,

    /// Path to the project's manifest file. If a directory is provided, the borneo.kdl file
    /// will be searched in it. Relative to base.
    #[arg(long)]
    pub manifest: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct BuildArgs {
    #[command(flatten)]
    pub project_args: ProjectArgs,

    /// Destination of the final artifact produced by the build. Relative to base.
    #[arg(long, short)]
    pub out: Option<PathBuf>,

    /// The packaging of the final artifact, possible values are: `jar`, `dir`. Default is `jar`.
    #[arg(long, short, value_enum)]
    pub packaging: Option<Packaging>,

    /// Entry class, overrides the manifest.
    #[arg(long, short)]
    pub entry: Option<String>,
}

#[derive(clap::Args)]
pub struct BuildCommand {
    #[command(flatten)]
    pub build_args: BuildArgs,
}

#[derive(clap::Args)]
pub struct SyncCommand {
    #[command(flatten)]
    pub project_args: ProjectArgs,
}

#[derive(clap::Args)]
pub struct CleanCommand {
    #[command(flatten)]
    pub project_args: ProjectArgs,

    /// Remove library artifacts not in the current lock
    #[arg(long)]
    pub purge: bool,
}

#[derive(clap::Args)]
pub struct TestCommand {
    #[command(flatten)]
    pub build_args: BuildArgs,

    #[arg(long)]
    pub class: Option<String>,
    #[arg(long)]
    pub method: Option<String>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub exclude_tag: Option<String>,
}

#[derive(clap::Args)]
pub struct RunCommand {
    #[command(flatten)]
    pub build_args: BuildArgs,

    #[arg(last = true)]
    pub args: Vec<String>,
}
