use std::path::PathBuf;

use clap::ValueHint;

use crate::manifest::Packaging;

#[derive(clap::Parser)]
#[command(version, about = "A build tool for Java projects")]
pub struct Cli {
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
    /// Remove build artifacts
    Clean(CleanCommand),
}

#[derive(clap::Args)]
pub struct ProjectArgs {
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    #[arg(long, short)]
    pub base: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct BuildArgs {
    #[command(flatten)]
    pub project_args: ProjectArgs,

    #[arg(long, short)]
    pub out: Option<PathBuf>,

    #[arg(long, short, value_enum)]
    pub packaging: Option<Packaging>,
}

#[derive(clap::Args)]
pub struct BuildCommand {
    #[command(flatten)]
    pub build_args: BuildArgs,
}

#[derive(clap::Args)]
pub struct CleanCommand {
    #[command(flatten)]
    pub project_args: ProjectArgs,

    /// Remove cached artifacts not in the current lock
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

    #[arg(long, short)]
    pub entry: Option<String>,

    #[arg(num_args = 1.., trailing_var_arg = true, value_hint = ValueHint::CommandWithArguments)]
    pub args: Vec<String>,
}
