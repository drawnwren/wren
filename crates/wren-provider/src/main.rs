use std::io;

fn main() {
    let result = wren_provider::serve(io::stdin().lock(), io::stdout().lock());
    if let Err(error) = result {
        eprintln!("wren-client-providers: {error}");
        std::process::exit(1);
    }
}
