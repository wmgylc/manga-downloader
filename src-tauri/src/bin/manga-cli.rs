fn main() {
    if let Err(err) = manga_downloader_lib::run_cli() {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}
