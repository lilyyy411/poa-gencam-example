use std::path::PathBuf;

use larpa::{Command, types::PrintVersion};

/// Camera thing or something idk
#[derive(Command, Debug, Clone)]
pub struct Cli {
    #[larpa(subcommand)]
    pub subcommand: Subcommand,

    /// Print version information.
    #[larpa(name = "--version", flag)]
    _version: PrintVersion,
}

#[derive(Command, Debug, Clone)]
pub enum Subcommand {
    /// Captures images and saves them
    Run(Run),
    /// Write the default config file to a path. This is a bit buggy sometimes.
    WriteConfig {
        /// The path to write the default config to. If not provided or --, writes to stdout.
        #[larpa(default = "--")]
        path: PathBuf,
    },
}

#[derive(Command, Debug, Clone)]
pub struct Run {
    /// The path to save images to
    #[larpa(name = ["-s", "--save-dir"], default = "data")]
    pub save_dir: PathBuf,
    /// The path to save logs to
    #[larpa(name = ["-l", "--log-dir"], default = "logs")]
    pub log_dir: PathBuf,
    /// The path to the KDL config file
    pub config: PathBuf,
}
