use std::net::SocketAddr;
use std::sync::Arc;

use kvlite_resp::{Decoder, Encoder, Frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

use crate::command::{Action, dispatch};
use crate::state::{ServerState, Session};

const READ_CHUNK: usize = 8 * 1024;

/// Serves one connection until the peer goes away or the server shuts down.
pub(crate) async fn serve(
    socket: TcpStream,
    peer: SocketAddr,
    state: Arc<ServerState>,
    shutdown: watch::Receiver<bool>,
) {
    let id = state.next_connection_id();
    let (outbox, inbox) = mpsc::unbounded_channel();
    let session = Session::new(id, peer, outbox);

    run(socket, session, &state, shutdown, inbox).await;

    // Whatever happened — clean exit, peer reset, protocol error — the subscriptions
    // go with the connection. Doing this here rather than at each return keeps it
    // impossible to forget.
    state.pubsub.disconnect(id);
}

async fn run(
    mut socket: TcpStream,
    mut session: Session,
    state: &ServerState,
    mut shutdown: watch::Receiver<bool>,
    mut inbox: mpsc::UnboundedReceiver<Frame>,
) {
    // Replies are small and latency matters more than packet count for a cache.
    let _ = socket.set_nodelay(true);

    let decoder = Decoder::new();
    let mut input = Vec::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut output = Vec::new();

    loop {
        // Drain every complete command already in the buffer before reading again.
        // That is what makes pipelining fast: one read, many commands, one write.
        output.clear();
        let mut consumed = 0;
        let mut closing = false;

        loop {
            match decoder.decode_command(&input[consumed..]) {
                Ok(None) => break,
                Ok(Some((args, used))) => {
                    consumed += used;
                    // An empty inline line is a no-op, not a command.
                    if args.is_empty() {
                        continue;
                    }
                    // The encoder is rebuilt per command because HELLO 3 changes the
                    // protocol mid-pipeline, and the reply to the next command must
                    // already use it.
                    match dispatch(&mut session, state, &args) {
                        Action::Reply(frames) => {
                            encode_all(session.protocol, &frames, &mut output);
                        }
                        Action::Close(frames) => {
                            encode_all(session.protocol, &frames, &mut output);
                            closing = true;
                            break;
                        }
                    }
                }
                Err(err) => {
                    // A protocol error means the stream can no longer be framed, so
                    // there is nothing to do but report and hang up.
                    encode_all(
                        session.protocol,
                        &[Frame::error(format!("ERR Protocol error: {err}"))],
                        &mut output,
                    );
                    closing = true;
                    break;
                }
            }
        }

        if consumed > 0 {
            input.drain(..consumed);
        }
        if !output.is_empty() && socket.write_all(&output).await.is_err() {
            return;
        }
        if closing {
            let _ = socket.flush().await;
            return;
        }

        tokio::select! {
            biased;

            _ = shutdown.changed() => return,

            Some(frame) = inbox.recv() => {
                let mut pushed = Vec::new();
                encode_all(session.protocol, &[frame], &mut pushed);
                // Coalesce a burst of deliveries into one write.
                while let Ok(frame) = inbox.try_recv() {
                    encode_all(session.protocol, &[frame], &mut pushed);
                }
                if socket.write_all(&pushed).await.is_err() {
                    return;
                }
            }

            read = socket.read(&mut chunk) => {
                match read {
                    Ok(0) | Err(_) => return,
                    Ok(count) => input.extend_from_slice(&chunk[..count]),
                }
            }
        }
    }
}

fn encode_all(protocol: kvlite_resp::RespProtocol, frames: &[Frame], out: &mut Vec<u8>) {
    let encoder = Encoder::new(protocol);
    for frame in frames {
        encoder.encode(frame, out);
    }
}
