//! O16: the third listener, plain `ws://` WebSocket, for browsers that use
//! `?transport=ws` (a same-machine A/B against WebTransport, and a fallback
//! for browsers without it). Off unless `--ws-bind` is given. No TLS: `wss` is
//! a follow-up, so this is for loopback and trusted networks. Library:
//! `tokio-tungstenite` (the usual tokio WebSocket, handshake only, no TLS
//! stack). Everything after the handshake goes through `handle_connection`
//! like every other peer, so rooms mix transports.

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use master_protocol::ws_frame::MAX_MESSAGE_BYTES;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Semaphore};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::peer_conn::PeerConnection;
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

impl WebSocketListener {
    /// Same admission rules as the QUIC accept loop in `serve`: per-address
    /// cap and the shared connection cap.
    pub async fn run(
        self,
        state: Arc<Mutex<ServiceState>>,
        next_connection_id: Arc<AtomicU64>,
        connection_slots: Arc<Semaphore>,
        address_counts: Arc<std::sync::Mutex<HashMap<IpAddr, usize>>>,
    ) {
        loop {
            let (tcp, remote) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    let _ = writeln!(std::io::stderr(), "websocket accept failed: {error}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Some(address_slot) = AddressSlot::claim(&address_counts, remote.ip()) else {
                let _ = writeln!(
                    std::io::stderr(),
                    "websocket refused: {remote} at per-address cap {MAX_CONNECTIONS_PER_ADDRESS}"
                );
                continue;
            };
            let Ok(permit) = connection_slots.clone().try_acquire_owned() else {
                let _ = writeln!(
                    std::io::stderr(),
                    "websocket refused: at connection cap {MAX_CONNECTIONS}"
                );
                continue;
            };
            let _ = tcp.set_nodelay(true);
            let state = Arc::clone(&state);
            let connection_id = next_connection_id.fetch_add(1, Ordering::Relaxed);
            let started = Instant::now();
            let _ = writeln!(
                std::io::stderr(),
                "connection {connection_id} incoming (websocket) remote={remote}"
            );
            tokio::spawn(async move {
                let _permit = permit;
                let _address_slot = address_slot;
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
                        handle_connection(state, connection_id, PeerConnection::WebSocket(peer))
                            .await;
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
            });
        }
    }
}
