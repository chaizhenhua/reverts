fn main() {
    if let Err(error) = reverts_cli::run(std::env::args().skip(1)) {
        eprintln!("{error}");
        eprintln!("{}", error.next_step());
        std::process::exit(1);
    }
}
