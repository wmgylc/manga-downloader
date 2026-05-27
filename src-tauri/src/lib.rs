pub mod cli;
mod config;
mod errors;
mod extensions;
mod jmcomic;
mod types;
mod utils;

pub fn run_cli() -> anyhow::Result<()> {
    cli::run()
}
