//! A minimal Redis client, shared by the wire test suites.
//!
//! Built from `kvlite-resp` alone, which doubles as evidence for that crate's
//! claim: a sans-io codec is all you need to talk to a Redis server.

// Each integration test is its own crate, so this module is compiled once per test
// binary and anything only one of them uses looks dead to the others.
#![allow(dead_code)]

use kvlite_resp::{Decoder, Encoder, Frame, RespProtocol};
use kvlite_testing::Server;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Client {
    stream: TcpStream,
    buffer: Vec<u8>,
    decoder: Decoder,
    pub protocol: RespProtocol,
}

impl Client {
    pub async fn connect(server: &Server) -> Self {
        let stream = TcpStream::connect(server.addr()).await.expect("connect");
        Self { stream, buffer: Vec::new(), decoder: Decoder::new(), protocol: RespProtocol::Resp2 }
    }

    pub fn encode(&self, args: &[&[u8]], out: &mut Vec<u8>) {
        let frame = Frame::Array(args.iter().map(Frame::bulk).collect());
        Encoder::new(self.protocol).encode(&frame, out);
    }

    pub async fn send(&mut self, args: &[&[u8]]) {
        let mut out = Vec::new();
        self.encode(args, &mut out);
        self.stream.write_all(&out).await.expect("write");
    }

    pub async fn call(&mut self, args: &[&[u8]]) -> Frame {
        self.send(args).await;
        self.read().await
    }

    /// Sends raw bytes, for exercising inline commands and malformed input.
    pub async fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.expect("write");
    }

    pub async fn read(&mut self) -> Frame {
        loop {
            if let Some((frame, consumed)) = self.decoder.decode(&self.buffer).expect("valid RESP")
            {
                self.buffer.drain(..consumed);
                return frame;
            }
            let mut chunk = [0u8; 4096];
            let count = self.stream.read(&mut chunk).await.expect("read");
            assert!(count > 0, "server closed the connection while a reply was pending");
            self.buffer.extend_from_slice(&chunk[..count]);
        }
    }

    /// Whether the server has closed the connection.
    pub async fn is_closed(&mut self) -> bool {
        let mut chunk = [0u8; 64];
        matches!(self.stream.read(&mut chunk).await, Ok(0) | Err(_))
    }
}

/// A frame's payload as text, for readable assertions and failure messages.
pub fn text(frame: &Frame) -> String {
    frame.to_string_lossy().unwrap_or_else(|| format!("{frame:?}"))
}

/// The bulk members of an array reply, sorted, as text.
///
/// Set replies have no defined order, so every assertion about one has to sort.
pub fn sorted_members(frame: &Frame) -> Vec<String> {
    let (Frame::Array(items) | Frame::Set(items)) = frame else {
        panic!("expected an array or set reply, got {frame:?}");
    };
    let mut members: Vec<String> = items.iter().map(text).collect();
    members.sort();
    members
}

/// The members of an array reply in the order they arrived, as text.
pub fn members(frame: &Frame) -> Vec<String> {
    let (Frame::Array(items) | Frame::Set(items)) = frame else {
        panic!("expected an array or set reply, got {frame:?}");
    };
    items.iter().map(text).collect()
}
