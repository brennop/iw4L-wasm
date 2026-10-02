//! One peer connection, whichever transport carried it: raw QUIC (native
//! clients, ALPN `iw4l-master/10`), WebTransport over HTTP/3 (browsers), or a
//! plain WebSocket (browsers with `?transport=ws`, O16).
//!
//! Method names follow `quinn` so the call sites in `main.rs` keep their shape;
//! only the types differ. Streams implement tokio's `AsyncRead`/`AsyncWrite`,
//! which is what the frame helpers in `main.rs` are generic over.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::Result;
use crate::relay_probe;
use crate::ws_peer::{WsPeer, WsRecv, WsSend};

/// How long `run_connection` waits for a WebTransport or WebSocket peer's
/// first Hello.
///
/// A browser joiner starts the handshake, then the page runs a blocking load
/// frame of about 10 s during which JS cannot open the bidi stream or write the
/// Hello. Temporary until O5 stops loading the map before joining.
pub const WEBTRANSPORT_HELLO_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub enum PeerConnection {
    Quic(quinn::Connection),
    WebTransport(wtransport::Connection),
    /// O16: a browser on the WebSocket transport (`ws_peer.rs`).
    WebSocket(WsPeer),
}

impl PeerConnection {
    /// Per-transport Hello deadline: native QUIC peers keep `HELLO_DEADLINE`
    /// (8 s); browser peers get longer (main-thread load stall).
    pub fn hello_deadline(&self) -> Duration {
        match self {
            Self::Quic(_) => crate::HELLO_DEADLINE,
            Self::WebTransport(_) | Self::WebSocket(_) => WEBTRANSPORT_HELLO_DEADLINE,
        }
    }

    pub async fn accept_bi(&self) -> Result<(PeerSend, PeerRecv)> {
        Ok(match self {
            Self::Quic(c) => {
                let (send, recv) = c.accept_bi().await?;
                (PeerSend::Quic(send), PeerRecv::Quic(recv))
            }
            Self::WebTransport(c) => {
                let (send, recv) = c.accept_bi().await?;
                (PeerSend::WebTransport(send), PeerRecv::WebTransport(recv))
            }
            Self::WebSocket(c) => {
                let (send, recv) = c.accept_bi().await?;
                (PeerSend::WebSocket(send), PeerRecv::WebSocket(recv))
            }
        })
    }

    pub async fn accept_uni(&self) -> Result<PeerRecv> {
        Ok(match self {
            Self::Quic(c) => PeerRecv::Quic(c.accept_uni().await?),
            Self::WebTransport(c) => PeerRecv::WebTransport(c.accept_uni().await?),
            Self::WebSocket(c) => PeerRecv::WebSocket(c.accept_uni().await?),
        })
    }

    pub async fn open_uni(&self) -> Result<PeerSend> {
        Ok(match self {
            Self::Quic(c) => PeerSend::Quic(c.open_uni().await?),
            Self::WebTransport(c) => PeerSend::WebTransport(c.open_uni().await?.await?),
            Self::WebSocket(c) => PeerSend::WebSocket(c.open_uni().await?),
        })
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Quic(_) => "quic",
            Self::WebTransport(_) => "wt",
            Self::WebSocket(_) => "ws",
        }
    }

    pub async fn read_datagram(&self) -> Result<Bytes> {
        let datagram = match self {
            Self::Quic(c) => c.read_datagram().await?,
            Self::WebTransport(c) => c.receive_datagram().await?.payload(),
            Self::WebSocket(c) => c.read_datagram().await?,
        };
        relay_probe::probe("M.recv", self.stable_id(), self.kind(), &datagram);
        Ok(datagram)
    }

    pub fn send_datagram(&self, data: Bytes) -> Result<()> {
        relay_probe::probe("M.send", self.stable_id(), self.kind(), &data);
        match self {
            Self::Quic(c) => c.send_datagram(data)?,
            Self::WebTransport(c) => c.send_datagram(data)?,
            Self::WebSocket(c) => c.send_datagram(&data)?,
        }
        Ok(())
    }

    pub fn close(&self, code: quinn::VarInt, reason: &[u8]) {
        match self {
            Self::Quic(c) => c.close(code, reason),
            Self::WebTransport(c) => c.close(
                wtransport::VarInt::from_u32(code.into_inner() as u32),
                reason,
            ),
            Self::WebSocket(c) => c.close(code.into_inner() as u32, reason),
        }
    }

    pub fn stable_id(&self) -> usize {
        match self {
            Self::Quic(c) => c.stable_id(),
            Self::WebTransport(c) => c.stable_id(),
            Self::WebSocket(c) => c.stable_id(),
        }
    }
}

pub enum PeerSend {
    Quic(quinn::SendStream),
    WebTransport(wtransport::SendStream),
    WebSocket(WsSend),
}

impl PeerSend {
    pub fn set_priority(&self, priority: i32) -> Result<()> {
        match self {
            Self::Quic(s) => s.set_priority(priority)?,
            Self::WebTransport(s) => s.set_priority(priority),
            Self::WebSocket(s) => s.set_priority(priority)?,
        }
        Ok(())
    }

    /// Queues the FIN and returns; does not wait for the peer to acknowledge
    /// (wtransport's own `finish` does, quinn's does not).
    pub fn finish(&mut self) -> Result<()> {
        match self {
            Self::Quic(s) => s.finish()?,
            Self::WebTransport(s) => s.quic_stream_mut().finish()?,
            Self::WebSocket(s) => s.finish()?,
        }
        Ok(())
    }
}

pub enum PeerRecv {
    Quic(quinn::RecvStream),
    WebTransport(wtransport::RecvStream),
    WebSocket(WsRecv),
}

impl PeerRecv {
    pub async fn read_to_end(&mut self, limit: usize) -> Result<Vec<u8>> {
        Ok(match self {
            Self::Quic(s) => s.read_to_end(limit).await?,
            Self::WebTransport(s) => s.quic_stream_mut().read_to_end(limit).await?,
            Self::WebSocket(s) => s.read_to_end(limit).await?,
        })
    }

    pub fn stop(&mut self, code: quinn::VarInt) -> Result<()> {
        match self {
            Self::Quic(s) => s.stop(code)?,
            Self::WebTransport(s) => s.quic_stream_mut().stop(code)?,
            Self::WebSocket(s) => s.stop(code.into_inner() as u32)?,
        }
        Ok(())
    }
}

impl AsyncRead for PeerRecv {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Quic(s) => Pin::new(s).poll_read(cx, buf),
            Self::WebTransport(s) => Pin::new(s).poll_read(cx, buf),
            Self::WebSocket(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for PeerSend {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Quic(s) => AsyncWrite::poll_write(Pin::new(s), cx, buf),
            Self::WebTransport(s) => AsyncWrite::poll_write(Pin::new(s), cx, buf),
            Self::WebSocket(s) => AsyncWrite::poll_write(Pin::new(s), cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Quic(s) => AsyncWrite::poll_flush(Pin::new(s), cx),
            Self::WebTransport(s) => AsyncWrite::poll_flush(Pin::new(s), cx),
            Self::WebSocket(s) => AsyncWrite::poll_flush(Pin::new(s), cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Quic(s) => AsyncWrite::poll_shutdown(Pin::new(s), cx),
            Self::WebTransport(s) => AsyncWrite::poll_shutdown(Pin::new(s), cx),
            Self::WebSocket(s) => AsyncWrite::poll_shutdown(Pin::new(s), cx),
        }
    }
}
