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
}

pub(super) trait MasterSendStream {
    /// Higher goes first when streams compete for the connection.
    fn set_priority(&self, priority: i32) -> Result<(), ConnError>;
    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), ConnError>;
    fn finish(&mut self) -> Result<(), ConnError>;
}

pub(super) trait MasterRecvStream {
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ConnError>;
    async fn read_to_end(&mut self, limit: usize) -> Result<Vec<u8>, ConnError>;
}

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
        Self(connection)
    }
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
        self.0
            .send_datagram(data.into())
            .map_err(ConnError::from_display)
    }

    async fn read_datagram(&self) -> Result<bytes::Bytes, ConnError> {
        self.0
            .read_datagram()
            .await
            .map_err(ConnError::from_display)
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
}
