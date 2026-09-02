fn main() {
    if let Err(error) = oce_client::cli::run_from(std::env::args_os()) {
        eprintln!("oce-client: {error}");
        std::process::exit(1);
    }
}
