fn main() {
    if let Err(error) = wren_extension::run_stdio_host(wren_extension::HostPlacement::Workspace) {
        eprintln!("wren workspace extension host: {error}");
        std::process::exit(1);
    }
}
