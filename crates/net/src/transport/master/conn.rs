//! The connection surface the master worker (`online.rs`) is written against:
//! one connection to the master carrying a control stream, datagrams and
//! bootstrap uni streams. The worker names `Conn`, `ConnSend` and `ConnRecv`,
//! never a transport, so a second backend (WebTransport in the browser) slots
//! in beside quinn without touching it.
//!
//! Method names and shapes follow quinn's, so the worker's call sites read the
//! same as plain quinn code. Every error is a `ConnError` that keeps the
//! backend's own message.

use std::fmt;
use std::ops::Deref;

// Declared here (not in `master.rs`) to keep that file untouched.
#[path = "hop_probe.rs"]
pub(super) mod hop_probe;

#[derive(Debug)]
pub(super) struct ConnError(String);

impl ConnError {
    pub(super) fn from_display(error: impl fmt::Display) -> Self {
        Self(error.to_string())
    }
}

impl fmt::Display for ConnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConnError {}

pub(super) trait MasterConn: Clone {
    type SendStream: MasterSendStream;
    type RecvStream: MasterRecvStream;
    type Datagram: Deref<Target = [u8]>;

    async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), ConnError>;
    async fn open_uni(&self) -> Result<Self::SendStream, ConnError>;
    async fn accept_uni(&self) -> Result<Self::RecvStream, ConnError>;
    fn send_datagram(&self, data: Vec<u8>) -> Result<(), ConnError>;
    async fn read_datagram(&self) -> Result<Self::Datagram, ConnError>;
    fn close(&self, code: u32, reason: &[u8]);
    /// Why the connection closed; `None` while it is open.
    fn close_reason(&self) -> Option<String>;
    /// The connection's measured round trip, where the transport reports one.
    fn rtt(&self) -> Option<std::time::Duration> {
        None
    }
    /// Congestion counters for the perf trace, where the transport has them.
    fn path_stats(&self) -> Option<PathStats> {
        None
    }
    /// Free bytes in the datagram send buffer, where the transport exposes it.
    fn datagram_send_buffer_space(&self) -> Option<usize> {
        None
    }
}

/// One sample of a connection's path: round trip, window, losses and the bytes
/// waiting in the datagram send buffer.
pub(super) struct PathStats {
    pub rtt_us: u64,
    pub cwnd: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
    pub queued: u64,
}

pub(super) trait MasterSendStream {
    /// Higher goes first when streams compete for the connection.
    fn set_priority(&self, priority: i32) -> Result<(), ConnError>;
    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), ConnError>;
    fn finish(&mut self) -> Result<(), ConnError>;

    /// Writes `bytes` and finishes the stream, failing when no write makes
    /// progress for `progress`. The caller bounds the whole transfer. The
    /// default (browser transports) has no per-write progress to observe.
    async fn write_transfer(
        &mut self,
        bytes: &[u8],
        _progress: std::time::Duration,
    ) -> Result<(), ConnError> {
        self.write_all(bytes).await?;
        self.finish()
    }

    /// Abandons an unfinished stream.
    fn reset(&mut self) {}
}

pub(super) trait MasterRecvStream {
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ConnError>;
    async fn read_to_end(&mut self, limit: usize) -> Result<Vec<u8>, ConnError>;

    /// `read_to_end`, failing when no chunk arrives for `progress`. The caller
    /// bounds the whole transfer. The default (browser transports) only has
    /// that overall bound.
    async fn read_transfer(
        &mut self,
        limit: usize,
        _progress: std::time::Duration,
    ) -> Result<Vec<u8>, ConnError> {
        self.read_to_end(limit).await
    }

    /// Tells the sender to stop; nothing more will be read.
    fn stop(&mut self) {}
}

/// Largest read or write a QUIC transfer makes at once.
#[cfg(not(target_arch = "wasm32"))]
const TRANSFER_CHUNK: usize = 16 * 1024;

#[cfg(not(target_arch = "wasm32"))]
pub(super) type Conn = QuicConn;
#[cfg(target_arch = "wasm32")]
pub(super) type Conn = super::conn_web::WebConn;
pub(super) type ConnSend = <Conn as MasterConn>::SendStream;
pub(super) type ConnRecv = <Conn as MasterConn>::RecvStream;

// Native backend: raw QUIC to the master with quinn. The browser backend
// (WebTransport) is `conn_web.rs`.

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub(super) struct QuicConn(quinn::Connection);

#[cfg(not(target_arch = "wasm32"))]
impl From<quinn::Connection> for QuicConn {
    fn from(connection: quinn::Connection) -> Self {
        if crate::client::pred_log::enabled() {
            watch_quic_stats(connection.clone());
        }
        Self(connection)
    }
}

/// O13: logs the connection's QUIC stats every 5 s until it closes. A no-op
/// outside a tokio runtime.
#[cfg(not(target_arch = "wasm32"))]
fn watch_quic_stats(connection: quinn::Connection) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tick.tick().await;
            if connection.close_reason().is_some() {
                return;
            }
            let stats = connection.stats();
            diag::info!(
                Net,
                "quic stats: conn={} kind=quic rtt_ms={} cwnd={} lost_packets={} congestion_events={} sent_datagrams={} recv_datagrams={}",
                connection.stable_id(),
                stats.path.rtt.as_millis(),
                stats.path.cwnd,
                stats.path.lost_packets,
                stats.path.congestion_events,
                stats.frame_tx.datagram,
                stats.frame_rx.datagram
            );
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) struct QuicSend(quinn::SendStream);

#[cfg(not(target_arch = "wasm32"))]
pub(super) struct QuicRecv(quinn::RecvStream);

#[cfg(not(target_arch = "wasm32"))]
impl MasterConn for QuicConn {
    type SendStream = QuicSend;
    type RecvStream = QuicRecv;
    type Datagram = bytes::Bytes;

    fn rtt(&self) -> Option<std::time::Duration> {
        Some(self.0.rtt())
    }

    fn path_stats(&self) -> Option<PathStats> {
        let stats = self.0.stats();
        Some(PathStats {
            rtt_us: stats.path.rtt.as_micros() as u64,
            cwnd: stats.path.cwnd,
            lost_packets: stats.path.lost_packets,
            congestion_events: stats.path.congestion_events,
            queued: super::online::DATAGRAM_SEND_BUFFER
                .saturating_sub(self.0.datagram_send_buffer_space()) as u64,
        })
    }

    fn datagram_send_buffer_space(&self) -> Option<usize> {
        Some(self.0.datagram_send_buffer_space())
    }

    async fn open_bi(&self) -> Result<(QuicSend, QuicRecv), ConnError> {
        let (send, recv) = self.0.open_bi().await.map_err(ConnError::from_display)?;
        Ok((QuicSend(send), QuicRecv(recv)))
    }

    async fn open_uni(&self) -> Result<QuicSend, ConnError> {
        self.0
            .open_uni()
            .await
            .map(QuicSend)
            .map_err(ConnError::from_display)
    }

    async fn accept_uni(&self) -> Result<QuicRecv, ConnError> {
        self.0
            .accept_uni()
            .await
            .map(QuicRecv)
            .map_err(ConnError::from_display)
    }

    fn send_datagram(&self, data: Vec<u8>) -> Result<(), ConnError> {
        hop_probe::probe("H.send", &data, "");
        self.0
            .send_datagram(data.into())
            .map_err(ConnError::from_display)
    }

    async fn read_datagram(&self) -> Result<bytes::Bytes, ConnError> {
        let datagram = self
            .0
            .read_datagram()
            .await
            .map_err(ConnError::from_display)?;
        hop_probe::probe("H.recv", &datagram, "");
        Ok(datagram)
    }

    fn close(&self, code: u32, reason: &[u8]) {
        self.0.close(code.into(), reason);
    }

    fn close_reason(&self) -> Option<String> {
        self.0.close_reason().map(|error| error.to_string())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl MasterSendStream for QuicSend {
    fn set_priority(&self, priority: i32) -> Result<(), ConnError> {
        self.0
            .set_priority(priority)
            .map_err(ConnError::from_display)
    }

    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), ConnError> {
        self.0
            .write_all(bytes)
            .await
            .map_err(ConnError::from_display)
    }

    fn finish(&mut self) -> Result<(), ConnError> {
        self.0.finish().map_err(ConnError::from_display)
    }

    async fn write_transfer(
        &mut self,
        bytes: &[u8],
        progress: std::time::Duration,
    ) -> Result<(), ConnError> {
        let mut offset = 0;
        while offset < bytes.len() {
            let end = (offset + TRANSFER_CHUNK).min(bytes.len());
            let written = tokio::time::timeout(progress, self.0.write(&bytes[offset..end]))
                .await
                .map_err(|_| ConnError::from_display("transfer progress deadline"))?
                .map_err(ConnError::from_display)?;
            if written == 0 {
                return Err(ConnError::from_display("transfer made no progress"));
            }
            offset += written;
        }
        self.finish()?;
        if let Some(code) = tokio::time::timeout(progress, self.0.stopped())
            .await
            .map_err(|_| ConnError::from_display("transfer acknowledgement deadline"))?
            .map_err(ConnError::from_display)?
        {
            return Err(ConnError::from_display(format!(
                "transfer refused with code {code}"
            )));
        }
        Ok(())
    }

    fn reset(&mut self) {
        let _ = self.0.reset(0_u8.into());
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl MasterRecvStream for QuicRecv {
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ConnError> {
        self.0
            .read_exact(buf)
            .await
            .map_err(ConnError::from_display)
    }

    async fn read_to_end(&mut self, limit: usize) -> Result<Vec<u8>, ConnError> {
        self.0
            .read_to_end(limit)
            .await
            .map_err(ConnError::from_display)
    }

    async fn read_transfer(
        &mut self,
        limit: usize,
        progress: std::time::Duration,
    ) -> Result<Vec<u8>, ConnError> {
        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::time::timeout(progress, self.0.read_chunk(TRANSFER_CHUNK, true))
                .await
                .map_err(|_| ConnError::from_display("transfer progress deadline"))?
                .map_err(ConnError::from_display)?;
            let Some(chunk) = chunk else {
                return Ok(bytes);
            };
            if chunk.bytes.is_empty() || bytes.len() + chunk.bytes.len() > limit {
                return Err(ConnError::from_display("transfer byte budget exceeded"));
            }
            bytes.extend_from_slice(&chunk.bytes);
        }
    }

    fn stop(&mut self) {
        let _ = self.0.stop(0_u8.into());
    }
}
