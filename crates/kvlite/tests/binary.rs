//! Runs the shipped binary and talks to it over a socket.
//!
//! Everything else tests the libraries. This tests the artifact a user actually
//! installs — that it parses its flags, binds the port it says it bound, and serves
//! RESP on it. Uses `std` only: if this needed a runtime, it would be testing the
//! test rather than the binary.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// A running `kvlite`, killed when the test ends however it ends.
struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kvlite"))
            .args(["--port", "0", "--databases", "4"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("the binary should be runnable");

        // The banner is the handshake: it reports the ephemeral port that was bound.
        let stdout = child.stdout.take().expect("piped");
        let mut banner = String::new();
        BufReader::new(stdout).read_line(&mut banner).expect("banner");

        let port = banner
            .split_whitespace()
            .find_map(|word| word.rsplit_once(':'))
            .and_then(|(_, port)| port.parse().ok())
            .unwrap_or_else(|| panic!("no port in banner: {banner:?}"));

        assert!(banner.contains("4 keyspaces"), "--databases was ignored: {banner:?}");
        Self { child, port }
    }

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        stream
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_binary_serves_resp_on_the_port_it_announces() {
    let server = Server::start();
    let mut stream = server.connect();

    // One write, five commands: the reply must come back pipelined and in order.
    stream
        .write_all(b"PING\r\nSET greeting hello\r\nGET greeting\r\nDBSIZE\r\nTYPE greeting\r\n")
        .expect("write");

    let expected = b"+PONG\r\n+OK\r\n$5\r\nhello\r\n:1\r\n+string\r\n";
    let mut actual = vec![0u8; expected.len()];
    stream.read_exact(&mut actual).expect("read the whole pipelined reply");

    assert_eq!(actual, expected, "got {:?}", String::from_utf8_lossy(&actual));
}

#[test]
fn the_binary_rejects_bad_arguments_instead_of_starting() {
    let output =
        Command::new(env!("CARGO_BIN_EXE_kvlite")).arg("--nonsense").output().expect("runnable");

    assert!(!output.status.success(), "a bad flag must not start a server");
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown option"));
}

#[test]
fn version_and_help_exit_cleanly() {
    for flag in ["--version", "--help"] {
        let output =
            Command::new(env!("CARGO_BIN_EXE_kvlite")).arg(flag).output().expect("runnable");
        assert!(output.status.success(), "{flag} should exit 0");
        assert!(!output.stdout.is_empty(), "{flag} should print something");
    }
}
