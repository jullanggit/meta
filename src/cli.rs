use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
pub struct Cli {
    #[arg(long, short)]
    /// The managers to run the command for
    pub managers: Option<Vec<String>>,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, PartialEq)]
pub enum Commands {
    /// Build the current configuration
    Build,
    /// Print the difference between the system and the config
    Diff,
    /// Upgrade all managers
    Upgrade,
}
