//! The Kvlite server binary.
//!
//! Argument parsing is hand-rolled. There are six flags, and a CLI framework would
//! cost every `cargo install kvlite` a proc-macro dependency and several seconds of
//! compile time to save forty lines. Reach for `clap` when the flags earn it.

use std::net::{IpAddr, SocketAddr};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use kvlite_core::{KvOptions, KvStore};
use kvlite_server::{Server, ServerConfig};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
kvlite — a Redis-compatible key-value server

USAGE:
    kvlite [OPTIONS]

OPTIONS:
    -b, --bind <ADDRESS>       Address to listen on            [default: 127.0.0.1]
    -p, --port <PORT>          Port to listen on, 0 for any    [default: 6380]
    -d, --databases <COUNT>    Number of keyspaces             [default: 16]
        --sweep-ms <MILLIS>    Expiry sweep interval, 0 to disable   [default: 100]
    -h, --help                 Print this help
    -V, --version              Print the version

Kvlite binds 127.0.0.1:6380 by default rather than the standard Redis port, so it
cannot quietly take over from a real Redis running on the same machine.
";

struct Options {
    bind: IpAddr,
    port: u16,
    databases: usize,
    sweep: Option<Duration>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            bind: IpAddr::from([127, 0, 0, 1]),
            port: 6380,
            databases: 16,
            sweep: Some(Duration::from_millis(100)),
        }
    }
}

/// `Ok(None)` means a flag was handled that should stop the program, such as `--help`.
fn parse(args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
    let mut options = Options::default();
    let mut args = args.peekable();

    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));

        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("kvlite {VERSION}");
                return Ok(None);
            }
            "-b" | "--bind" => {
                let raw = value()?;
                options.bind = raw.parse().map_err(|_| format!("not an IP address: {raw}"))?;
            }
            "-p" | "--port" => {
                let raw = value()?;
                options.port = raw.parse().map_err(|_| format!("not a port: {raw}"))?;
            }
            "-d" | "--databases" => {
                let raw = value()?;
                let count: usize = raw.parse().map_err(|_| format!("not a number: {raw}"))?;
                if count == 0 {
                    return Err("--databases must be at least 1".into());
                }
                options.databases = count;
            }
            "--sweep-ms" => {
                let raw = value()?;
                let millis: u64 = raw.parse().map_err(|_| format!("not a number: {raw}"))?;
                options.sweep = (millis > 0).then(|| Duration::from_millis(millis));
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }

    Ok(Some(options))
}

fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("kvlite: {message}\n\nTry 'kvlite --help'.");
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("kvlite: could not start the runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(run(options))
}

async fn run(options: Options) -> ExitCode {
    let mut store_options = KvOptions::default();
    store_options.keyspace_count = options.databases;
    let store = Arc::new(KvStore::with_options(store_options));

    let mut config = ServerConfig::default();
    config.bind = SocketAddr::new(options.bind, options.port);
    config.expiry_sweep_interval = options.sweep;

    let server = match Server::bind_with_store(config, store).await {
        Ok(server) => server,
        Err(err) => {
            eprintln!("kvlite: could not bind {}:{}: {err}", options.bind, options.port);
            return ExitCode::FAILURE;
        }
    };

    println!(
        "kvlite {VERSION} ready on {} with {} keyspaces",
        server.local_addr(),
        options.databases
    );

    match tokio::signal::ctrl_c().await {
        Ok(()) => println!("\nkvlite: shutting down"),
        Err(err) => eprintln!("kvlite: cannot listen for shutdown signals: {err}"),
    }

    server.shutdown().await;
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Option<Options>, String> {
        parse(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn defaults_avoid_the_real_redis_port() {
        let options = parse_args(&[]).unwrap().unwrap();
        assert_eq!(options.port, 6380, "6379 belongs to whatever is already running");
        assert_eq!(options.bind, IpAddr::from([127, 0, 0, 1]), "loopback, not the world");
        assert_eq!(options.databases, 16);
    }

    #[test]
    fn accepts_short_and_long_flags() {
        let options = parse_args(&["-p", "0", "--bind", "0.0.0.0", "-d", "4"]).unwrap().unwrap();
        assert_eq!(options.port, 0);
        assert_eq!(options.bind, IpAddr::from([0, 0, 0, 0]));
        assert_eq!(options.databases, 4);
    }

    #[test]
    fn a_zero_sweep_interval_disables_the_sweep() {
        assert_eq!(parse_args(&["--sweep-ms", "0"]).unwrap().unwrap().sweep, None);
        assert_eq!(
            parse_args(&["--sweep-ms", "250"]).unwrap().unwrap().sweep,
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn rejects_nonsense_rather_than_guessing() {
        assert!(parse_args(&["--nope"]).is_err());
        assert!(parse_args(&["--port"]).is_err(), "a flag with no value");
        assert!(parse_args(&["--port", "not-a-port"]).is_err());
        assert!(parse_args(&["--bind", "300.1.1.1"]).is_err());
        assert!(parse_args(&["--databases", "0"]).is_err(), "a store needs a keyspace");
    }

    #[test]
    fn help_and_version_stop_without_starting_a_server() {
        assert!(parse_args(&["--help"]).unwrap().is_none());
        assert!(parse_args(&["-V"]).unwrap().is_none());
    }
}
