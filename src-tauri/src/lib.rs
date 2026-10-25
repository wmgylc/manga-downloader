pub mod cli;
mod config;
mod errors;
mod extensions;
mod jmcomic;
mod types;
pub mod utils;

pub use types::{ComicCollection, ComicEpisode};

pub fn run_cli() -> anyhow::Result<()> {
    cli::run()
}
