//! O16: the third listener, plain `ws://` WebSocket, for browsers that use
//! `?transport=ws` (a same-machine A/B against WebTransport, and a fallback
//! for browsers without it). Off unless `--ws-bind` is given. No TLS: `wss` is
//! a follow-up, so this is for loopback and trusted networks. Library:
//! `tokio-tungstenite` (the usual tokio WebSocket, handshake only, no TLS
//! stack). Everything after the handshake goes through `handle_connection`
//! like every other peer, so rooms mix transports.
//!
//! D3a: with `--web-root` the same port also serves the web build over HTTP
//! (`web_static.rs`). Each connection's request head is peeked and parsed;
//! `Upgrade: websocket` takes the game path below, anything else the static
//! handler. Without `--web-root` nothing is peeked and the behaviour is O16's.

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use master_protocol::ws_frame::MAX_MESSAGE_BYTES;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::peer_conn::PeerConnection;
use crate::web_static::{self, Head, Parsed, WebConfig};
use crate::ws_peer::WsPeer;
use crate::{
    AddressSlot, HELLO_DEADLINE, MAX_CONNECTIONS, MAX_CONNECTIONS_PER_ADDRESS, Result,
    ServiceState, handle_connection,
};

pub struct WebSocketListener {
    listener: TcpListener,
}

/// Binds the TCP port.
pub async fn bind(addr: SocketAddr) -> Result<WebSocketListener> {
    let listener = TcpListener::bind(addr).await?;
    writeln!(
        std::io::stderr(),
        "iw4l-master websocket listening on {} (plain ws://, no TLS)",
        listener.local_addr()?
    )?;
    Ok(WebSocketListener { listener })
}

/// What the first request on a connection is.
enum Classified {
    Websocket,
    Http(Head),
    /// Unparseable or oversized head: answered with 400.
    Bad,
    /// Closed, errored or too slow: dropped silently.
    Gone,
}

/// Peeks (consumes nothing) until the request head is complete.
async fn classify(tcp: &TcpStream) -> Classified {
    let mut buf = vec![0_u8; web_static::MAX_HEAD_BYTES];
    let waited = tokio::time::timeout(web_static::HEAD_DEADLINE, async {
        loop {
            let Ok(read) = tcp.peek(&mut buf).await else {
                return Classified::Gone;
            };
            if read == 0 {
                return Classified::Gone;
            }
            match web_static::parse_head(&buf[..read]) {
                Parsed::Complete(head) if head.upgrade_websocket => return Classified::Websocket,
                Parsed::Complete(head) => return Classified::Http(head),
                Parsed::Bad => return Classified::Bad,
                Parsed::Partial if read == buf.len() => return Classified::Bad,
                // `peek` returns at once with what is buffered, so poll.
                Parsed::Partial => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await;
    waited.unwrap_or(Classified::Gone)
}

impl WebSocketListener {
    #[cfg(test)]
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Same admission rules as the QUIC accept loop in `serve`: per-address
    /// cap and the shared connection cap. With `web`, a connection first
    /// takes an HTTP slot (a separate pool, see `web_static`) and only a
    /// request that turns out to be a websocket upgrade claims the player
    /// caps, so page loads never use them.
    pub async fn run(
        self,
        state: Arc<Mutex<ServiceState>>,
        next_connection_id: Arc<AtomicU64>,
        connection_slots: Arc<Semaphore>,
        address_counts: Arc<std::sync::Mutex<HashMap<IpAddr, usize>>>,
        web: Option<Arc<WebConfig>>,
    ) {
        let http_slots = Arc::new(Semaphore::new(web_static::MAX_HTTP_CONNECTIONS));
        loop {
            let (tcp, remote) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    let _ = writeln!(std::io::stderr(), "websocket accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let _ = tcp.set_nodelay(true);
            let state = Arc::clone(&state);
            let next_connection_id = Arc::clone(&next_connection_id);
            let connection_slots = Arc::clone(&connection_slots);
            let address_counts = Arc::clone(&address_counts);
            let Some(web) = web.clone() else {
                // O16 path: admission before the handshake, as before.
                let Some(admission) = admit(&connection_slots, &address_counts, remote) else {
                    continue;
                };
                let connection_id = next_connection_id.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let _admission = admission;
                    game_connection(tcp, remote, state, connection_id).await;
                });
                continue;
            };
            let Ok(http_permit) = http_slots.clone().try_acquire_owned() else {
                let _ = writeln!(
                    std::io::stderr(),
                    "websocket refused: {remote} at http connection cap {}",
                    web_static::MAX_HTTP_CONNECTIONS
                );
                continue;
            };
            tokio::spawn(async move {
                let mut tcp = tcp;
                match classify(&tcp).await {
                    Classified::Websocket => {
                        // Leave the HTTP pool, take a player slot.
                        drop(http_permit);
                        let Some(admission) = admit(&connection_slots, &address_counts, remote)
                        else {
                            return;
                        };
                        let _admission = admission;
                        let connection_id = next_connection_id.fetch_add(1, Ordering::Relaxed);
                        game_connection(tcp, remote, state, connection_id).await;
                    }
                    Classified::Http(head) => {
                        let _http_permit = http_permit;
                        let started = Instant::now();
                        let outcome = web_static::serve(&mut tcp, &head, &web, &state).await;
                        let _ = writeln!(
                            std::io::stderr(),
                            "http {} {} remote={remote} {} elapsed_ms={}",
                            head.method,
                            head.target,
                            match outcome {
                                Ok((status, bytes)) => format!("-> {status} bytes={bytes}"),
                                Err(error) => format!("failed: {error}"),
                            },
                            started.elapsed().as_millis()
                        );
                    }
                    Classified::Bad => {
                        let _ = web_static::bad_request(&mut tcp).await;
                    }
                    Classified::Gone => {}
                }
            });
        }
    }
}

/// The player caps held for a connection's lifetime.
struct Admission {
    _permit: OwnedSemaphorePermit,
    _address_slot: AddressSlot,
}

fn admit(
    connection_slots: &Arc<Semaphore>,
    address_counts: &Arc<std::sync::Mutex<HashMap<IpAddr, usize>>>,
    remote: SocketAddr,
) -> Option<Admission> {
    let Some(address_slot) = AddressSlot::claim(address_counts, remote.ip()) else {
        let _ = writeln!(
            std::io::stderr(),
            "websocket refused: {remote} at per-address cap {MAX_CONNECTIONS_PER_ADDRESS}"
        );
        return None;
    };
    let Ok(permit) = connection_slots.clone().try_acquire_owned() else {
        let _ = writeln!(
            std::io::stderr(),
            "websocket refused: at connection cap {MAX_CONNECTIONS}"
        );
        return None;
    };
    Some(Admission {
        _permit: permit,
        _address_slot: address_slot,
    })
}

/// The WebSocket handshake and everything after it, for an admitted connection.
async fn game_connection(
    tcp: TcpStream,
    remote: SocketAddr,
    state: Arc<Mutex<ServiceState>>,
    connection_id: u64,
) {
    let started = Instant::now();
    let _ = writeln!(
        std::io::stderr(),
        "connection {connection_id} incoming (websocket) remote={remote}"
    );
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let handshake = tokio::time::timeout(
        HELLO_DEADLINE,
        tokio_tungstenite::accept_async_with_config(tcp, Some(config)),
    )
    .await;
    match handshake {
        Ok(Ok(socket)) => {
            let _ = writeln!(
                std::io::stderr(),
                "connection {connection_id} handshake ok (websocket) remote={remote} elapsed_ms={}",
                started.elapsed().as_millis()
            );
            let peer = WsPeer::new(socket, connection_id as usize);
            handle_connection(state, connection_id, PeerConnection::WebSocket(peer)).await;
        }
        Ok(Err(error)) => {
            let _ = writeln!(
                std::io::stderr(),
                "connection {connection_id} handshake failed (websocket) remote={remote} elapsed_ms={}: {error}",
                started.elapsed().as_millis()
            );
        }
        Err(_) => {
            let _ = writeln!(
                std::io::stderr(),
                "connection {connection_id} handshake failed (websocket) remote={remote}: deadline"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One listener, one port: `GET /index.html` and a websocket handshake.
    #[tokio::test]
    async fn serves_http_and_websocket_on_one_port() {
        let root = std::env::temp_dir().join(format!("iw4l-ws-web-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.html"), "<html>page</html>").unwrap();
        let web = Arc::new(WebConfig {
            root: root.canonicalize().unwrap(),
            wt: None,
        });
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        tokio::spawn(listener.run(
            Arc::new(Mutex::new(ServiceState::default())),
            Arc::new(AtomicU64::new(1)),
            Arc::clone(&slots),
            Arc::new(std::sync::Mutex::new(HashMap::new())),
            Some(web),
        ));

        let mut http = TcpStream::connect(addr).await.unwrap();
        http.write_all(b"GET /index.html HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        http.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("<html>page</html>"), "{response}");

        let tcp = TcpStream::connect(addr).await.unwrap();
        let (_socket, reply) = tokio_tungstenite::client_async(format!("ws://{addr}/"), tcp)
            .await
            .unwrap();
        assert_eq!(reply.status(), 101);
        // The HTTP request held no player slot; the ws connection holds one.
        assert_eq!(slots.available_permits(), MAX_CONNECTIONS - 1);
    }
}
