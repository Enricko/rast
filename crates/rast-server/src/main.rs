//! `rast-server [--listen 127.0.0.1:8723] [--data /var/lib/rast-revamp]`
//!
//! Serves the account API under `/api`. nginx serves the web client and
//! forwards `/api/` here; the server never listens on a public address itself.

use std::net::SocketAddr;
use std::path::PathBuf;

const USAGE: &str = "usage: rast-server [--listen ADDR] [--data DIR]\n\
    --listen ADDR  address to serve the API on (default 127.0.0.1:8723, env RAST_LISTEN)\n\
    --data DIR     directory for the account database (default ./rast-data, env RAST_DATA)";

fn main() {
    let mut listen = std::env::var("RAST_LISTEN").unwrap_or_else(|_| "127.0.0.1:8723".to_owned());
    let mut data =
        std::env::var_os("RAST_DATA").map_or_else(|| PathBuf::from("rast-data"), PathBuf::from);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                listen = args
                    .next()
                    .unwrap_or_else(|| fail("--listen needs an address"))
            }
            "--data" => {
                data = PathBuf::from(
                    args.next()
                        .unwrap_or_else(|| fail("--data needs a directory")),
                )
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => fail(&format!("unknown argument: {other}")),
        }
    }
    let listen: SocketAddr = listen
        .parse()
        .unwrap_or_else(|_| fail(&format!("not an address: {listen}")));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| fail(&format!("cannot start the runtime: {error}")));
    if let Err(error) = runtime.block_on(rast_server::serve(listen, data)) {
        fail(&error);
    }
}

fn fail(message: &str) -> ! {
    eprintln!("rast-server: {message}\n{USAGE}");
    std::process::exit(2);
}
