fn main() {
    let arguments = oce_client::cli::legacy_mcp_arguments(std::env::args_os());
    if let Err(error) = oce_client::cli::run_from(arguments) {
        eprintln!("oce-client: {error}");
        std::process::exit(1);
    }
}
