//! O16: one WebSocket peer (a browser using `?transport=ws`) presented as the
//! same things a QUIC peer has: datagrams, an accepted bidi stream (control),
//! master-opened uni streams (bootstrap), priorities and a close.
//!
//! The wire format is `master_protocol::ws_frame`. One reader task decodes
//! messages into per-stream channels and the datagram queue; one writer task
//! owns the socket's sink and is fed by two bounded channels, `hi` (datagrams,
//! the control stream, stream opens, FINs of those) and `lo` (bootstrap
//! chunks, priority below 0). The writer drains `hi` first. Streams of the same
//! priority share a channel, so their 16 KiB chunks go out in arrival order,
//! which is round-robin between writers that each wait for a free slot.
//!
//! Datagrams are droppable: a datagram is dropped, and counted, when more than
//! `DATAGRAM_QUEUE_MAX` frames already wait for the socket, so TCP
//! back-pressure never becomes seconds of stale relay traffic. Stream frames
//! wait and are never dropped. TCP resends lost packets, so one loss delays
//! everything behind it; loopback has none, so that cost is not measured here.

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use master_protocol::ws_frame::{self, MAX_DATA_CHUNK, WsFrame};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::{CancellationToken, PollSender};

use crate::Result;

/// Frames the writer may have queued (both priorities each).
const HI_CAP: usize = 256;
const LO_CAP: usize = 64;
/// A datagram is dropped when more frames than this already wait in `hi`.
pub const DATAGRAM_QUEUE_MAX: usize = 64;
/// Decoded upstream datagrams waiting for the relay loop.
const DATAGRAM_IN_CAP: usize = 256;
const STREAM_IN_CAP: usize = 32;
/// Client-opened streams a peer may hold at once.
const MAX_CLIENT_STREAMS: usize = 8;

struct Shared {
    id: usize,
    hi: mpsc::Sender<Vec<u8>>,
    lo: mpsc::Sender<Vec<u8>>,
    kill: CancellationToken,
    next_uni: AtomicU32,
    /// Send-side "the reader stopped this stream" flags, by stream id.
    stops: StdMutex<HashMap<u32, Arc<AtomicBool>>>,
    datagrams: Mutex<mpsc::Receiver<Bytes>>,
    bi: Mutex<mpsc::Receiver<(WsSend, WsRecv)>>,
    uni: Mutex<mpsc::Receiver<WsRecv>>,
    sent_datagrams: AtomicU64,
    recv_datagrams: AtomicU64,
    dropped_out: AtomicU64,
    dropped_in: AtomicU64,
    queued_max: AtomicUsize,
}

/// Counters for the relay probe's periodic line.
pub struct WsStats {
    pub sent_datagrams: u64,
    pub recv_datagrams: u64,
    pub dropped_out: u64,
    pub dropped_in: u64,
    pub queued_max: usize,
}

#[derive(Clone)]
pub struct WsPeer(Arc<Shared>);

impl WsPeer {
    /// Starts the reader and writer tasks for an accepted WebSocket. Needs a
    /// tokio runtime.
    pub fn new<S>(socket: WebSocketStream<S>, id: usize) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sink, stream) = socket.split();
        let (hi, hi_rx) = mpsc::channel(HI_CAP);
        let (lo, lo_rx) = mpsc::channel(LO_CAP);
        let (datagram_tx, datagram_rx) = mpsc::channel(DATAGRAM_IN_CAP);
        let (bi_tx, bi_rx) = mpsc::channel(MAX_CLIENT_STREAMS);
        let (uni_tx, uni_rx) = mpsc::channel(1);
        let kill = CancellationToken::new();
        let shared = Arc::new(Shared {
            id,
            hi,
            lo,
            kill: kill.clone(),
            next_uni: AtomicU32::new(1),
            stops: StdMutex::new(HashMap::new()),
            datagrams: Mutex::new(datagram_rx),
            bi: Mutex::new(bi_rx),
            uni: Mutex::new(uni_rx),
            sent_datagrams: AtomicU64::new(0),
            recv_datagrams: AtomicU64::new(0),
            dropped_out: AtomicU64::new(0),
            dropped_in: AtomicU64::new(0),
            queued_max: AtomicUsize::new(0),
        });
        tokio::spawn(write_loop(sink, hi_rx, lo_rx, kill.clone()));
        tokio::spawn(read_loop(
            Arc::clone(&shared),
            stream,
            datagram_tx,
            bi_tx,
            uni_tx,
        ));
        Self(shared)
    }

    pub fn stable_id(&self) -> usize {
        self.0.id
    }

    pub fn stats(&self) -> WsStats {
        WsStats {
            sent_datagrams: self.0.sent_datagrams.load(Ordering::Relaxed),
            recv_datagrams: self.0.recv_datagrams.load(Ordering::Relaxed),
            dropped_out: self.0.dropped_out.load(Ordering::Relaxed),
            dropped_in: self.0.dropped_in.load(Ordering::Relaxed),
            queued_max: self.0.queued_max.load(Ordering::Relaxed),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.0.kill.is_cancelled()
    }

    pub async fn accept_bi(&self) -> Result<(WsSend, WsRecv)> {
        self.0
            .bi
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| "websocket closed".into())
    }

    /// Clients never open uni streams on this transport; this waits until the
    /// connection closes.
    pub async fn accept_uni(&self) -> Result<WsRecv> {
        self.0
            .uni
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| "websocket closed".into())
    }

    pub async fn open_uni(&self) -> Result<WsSend> {
        let id = self.0.next_uni.fetch_add(2, Ordering::Relaxed);
        let stopped = Arc::new(AtomicBool::new(false));
        self.0
            .stops
            .lock()
            .expect("ws stops poisoned")
            .insert(id, Arc::clone(&stopped));
        self.0
            .hi
            .send(WsFrame::OpenUni { stream: id }.encode())
            .await
            .map_err(|_| "websocket closed")?;
        Ok(WsSend::new(Arc::clone(&self.0), id, stopped))
    }

    pub async fn read_datagram(&self) -> Result<Bytes> {
        self.0
            .datagrams
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| "websocket closed".into())
    }

    /// Queues a datagram, or drops it when too many frames wait for the socket.
    pub fn send_datagram(&self, data: &[u8]) -> Result<()> {
        let shared = &self.0;
        if shared.kill.is_cancelled() {
            return Err("websocket closed".into());
        }
        let queued = HI_CAP - shared.hi.capacity();
        if queued > DATAGRAM_QUEUE_MAX {
            shared.dropped_out.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        shared.queued_max.fetch_max(queued, Ordering::Relaxed);
        match shared.hi.try_send(WsFrame::Datagram(data).encode()) {
            Ok(()) => {
                shared.sent_datagrams.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                shared.dropped_out.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err("websocket closed".into()),
        }
    }

    pub fn close(&self, code: u32, reason: &[u8]) {
        let frame = WsFrame::Close { code, reason }.encode();
        if self.0.hi.try_send(frame).is_err() {
            self.0.kill.cancel();
        }
    }
}

async fn write_loop<S>(
    mut sink: futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    mut hi: mpsc::Receiver<Vec<u8>>,
    mut lo: mpsc::Receiver<Vec<u8>>,
    kill: CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    loop {
        let frame = tokio::select! {
            biased;
            () = kill.cancelled() => break,
            frame = hi.recv() => frame,
            frame = lo.recv() => frame,
        };
        let Some(frame) = frame else { break };
        let closing = frame.first() == Some(&ws_frame::KIND_CLOSE);
        if sink.send(Message::Binary(frame.into())).await.is_err() {
            break;
        }
        if closing {
            let _ = sink.close().await;
            break;
        }
    }
    kill.cancel();
}

async fn read_loop<S>(
    shared: Arc<Shared>,
    mut stream: SplitStream<WebSocketStream<S>>,
    datagram_tx: mpsc::Sender<Bytes>,
    bi_tx: mpsc::Sender<(WsSend, WsRecv)>,
    _uni_tx: mpsc::Sender<WsRecv>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let kill = shared.kill.clone();
    let mut streams: HashMap<u32, mpsc::Sender<Bytes>> = HashMap::new();
    loop {
        let message = tokio::select! {
            () = kill.cancelled() => break,
            message = stream.next() => message,
        };
        let Some(Ok(message)) = message else { break };
        let bytes = match message {
            Message::Binary(bytes) => bytes,
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => continue,
            _ => break,
        };
        let Ok(frame) = ws_frame::decode(&bytes) else {
            break;
        };
        match frame {
            WsFrame::Datagram(payload) => {
                shared.recv_datagrams.fetch_add(1, Ordering::Relaxed);
                if datagram_tx
                    .try_send(Bytes::copy_from_slice(payload))
                    .is_err()
                {
                    shared.dropped_in.fetch_add(1, Ordering::Relaxed);
                }
            }
            WsFrame::OpenBi { stream: id } => {
                if id % 2 != 0 || streams.contains_key(&id) || streams.len() >= MAX_CLIENT_STREAMS {
                    break;
                }
                let (tx, rx) = mpsc::channel(STREAM_IN_CAP);
                streams.insert(id, tx);
                let stopped = Arc::new(AtomicBool::new(false));
                shared
                    .stops
                    .lock()
                    .expect("ws stops poisoned")
                    .insert(id, Arc::clone(&stopped));
                let pair = (
                    WsSend::new(Arc::clone(&shared), id, stopped),
                    WsRecv::new(Arc::clone(&shared), id, rx),
                );
                if bi_tx.try_send(pair).is_err() {
                    break;
                }
            }
            WsFrame::OpenUni { .. } => break,
            WsFrame::Data {
                stream: id,
                payload,
            } => {
                if let Some(tx) = streams.get(&id)
                    && tx.send(Bytes::copy_from_slice(payload)).await.is_err()
                {
                    streams.remove(&id);
                }
            }
            WsFrame::Fin { stream: id } => {
                streams.remove(&id);
            }
            WsFrame::Stop { stream: id, .. } => {
                if let Some(flag) = shared.stops.lock().expect("ws stops poisoned").get(&id) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            WsFrame::Close { .. } => break,
        }
    }
    kill.cancel();
}

/// The write half of a stream: `AsyncWrite` over the peer's writer channels.
pub struct WsSend {
    shared: Arc<Shared>,
    id: u32,
    stopped: Arc<AtomicBool>,
    priority: AtomicI32,
    hi: PollSender<Vec<u8>>,
    lo: PollSender<Vec<u8>>,
    finished: bool,
}

impl WsSend {
    fn new(shared: Arc<Shared>, id: u32, stopped: Arc<AtomicBool>) -> Self {
        Self {
            hi: PollSender::new(shared.hi.clone()),
            lo: PollSender::new(shared.lo.clone()),
            shared,
            id,
            stopped,
            priority: AtomicI32::new(0),
            finished: false,
        }
    }

    pub fn set_priority(&self, priority: i32) -> Result<()> {
        self.priority.store(priority, Ordering::Relaxed);
        Ok(())
    }

    fn low(&self) -> bool {
        self.priority.load(Ordering::Relaxed) < 0
    }

    /// Queues the FIN behind the data already queued on this stream's channel.
    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let frame = WsFrame::Fin { stream: self.id }.encode();
        let tx = if self.low() {
            self.shared.lo.clone()
        } else {
            self.shared.hi.clone()
        };
        match tx.try_send(frame) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(frame)) => {
                tokio::spawn(async move {
                    let _ = tx.send(frame).await;
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return Err("websocket closed".into()),
        }
        Ok(())
    }
}

fn broken(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, reason)
}

impl AsyncWrite for WsSend {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if this.finished {
            return Poll::Ready(Err(broken("stream finished")));
        }
        if this.stopped.load(Ordering::Relaxed) {
            return Poll::Ready(Err(broken("stream stopped by the peer")));
        }
        let take = buf.len().min(MAX_DATA_CHUNK);
        let frame = WsFrame::Data {
            stream: this.id,
            payload: &buf[..take],
        }
        .encode();
        let sender = if this.low() {
            &mut this.lo
        } else {
            &mut this.hi
        };
        ready!(sender.poll_reserve(cx)).map_err(|_| broken("websocket closed"))?;
        sender
            .send_item(frame)
            .map_err(|_| broken("websocket closed"))?;
        Poll::Ready(Ok(take))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(
            self.get_mut()
                .finish()
                .map_err(|_| broken("websocket closed")),
        )
    }
}

/// The read half of a stream: `AsyncRead` over the reader task's channel.
pub struct WsRecv {
    shared: Arc<Shared>,
    id: u32,
    rx: mpsc::Receiver<Bytes>,
    chunk: Bytes,
}

impl WsRecv {
    fn new(shared: Arc<Shared>, id: u32, rx: mpsc::Receiver<Bytes>) -> Self {
        Self {
            shared,
            id,
            rx,
            chunk: Bytes::new(),
        }
    }

    pub async fn read_to_end(&mut self, limit: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        if !self.chunk.is_empty() {
            out.extend_from_slice(&self.chunk.split_off(0));
        }
        while let Some(chunk) = self.rx.recv().await {
            out.extend_from_slice(&chunk);
            if out.len() > limit {
                return Err(format!("stream longer than {limit} bytes").into());
            }
        }
        if out.len() > limit {
            return Err(format!("stream longer than {limit} bytes").into());
        }
        Ok(out)
    }

    /// Tells the sender to stop and discards what arrives.
    pub fn stop(&mut self, code: u32) -> Result<()> {
        self.rx.close();
        let frame = WsFrame::Stop {
            stream: self.id,
            code,
        }
        .encode();
        let _ = self.shared.hi.try_send(frame);
        Ok(())
    }
}

impl AsyncRead for WsRecv {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if !this.chunk.is_empty() {
                let take = this.chunk.len().min(buf.remaining());
                buf.put_slice(&this.chunk.split_to(take));
                return Poll::Ready(Ok(()));
            }
            match ready!(this.rx.poll_recv(cx)) {
                Some(chunk) => this.chunk = chunk,
                None => return Poll::Ready(Ok(())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer_conn::PeerConnection;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::{accept_async, client_async};

    type Client = WebSocketStream<TcpStream>;

    async fn pair() -> (WsPeer, Client) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            tcp.set_nodelay(true).unwrap();
            WsPeer::new(accept_async(tcp).await.unwrap(), 7)
        });
        let tcp = TcpStream::connect(addr).await.unwrap();
        let (client, _) = client_async(format!("ws://{addr}/"), tcp).await.unwrap();
        (server.await.unwrap(), client)
    }

    async fn send(client: &mut Client, frame: WsFrame<'_>) {
        client
            .send(Message::Binary(frame.encode().into()))
            .await
            .unwrap();
    }

    async fn next(client: &mut Client) -> Vec<u8> {
        match client.next().await.unwrap().unwrap() {
            Message::Binary(bytes) => bytes.to_vec(),
            other => panic!("unexpected message {other:?}"),
        }
    }

    #[tokio::test]
    async fn streams_datagrams_and_close() {
        let (peer, mut client) = pair().await;
        // Through PeerConnection, as handle_connection uses it.
        let peer = PeerConnection::WebSocket(peer);
        assert_eq!(peer.stable_id(), 7);
        assert_eq!(peer.kind(), "ws");

        // Client opens the control stream, sends a Hello-sized payload and a datagram.
        let hello = vec![0xab_u8; 100];
        send(&mut client, WsFrame::OpenBi { stream: 0 }).await;
        send(
            &mut client,
            WsFrame::Data {
                stream: 0,
                payload: &hello,
            },
        )
        .await;
        send(&mut client, WsFrame::Datagram(b"up datagram")).await;

        let (mut control_tx, mut control_rx) = peer.accept_bi().await.unwrap();
        let mut got = vec![0_u8; 100];
        control_rx.read_exact(&mut got).await.unwrap();
        assert_eq!(got, hello);
        assert_eq!(&peer.read_datagram().await.unwrap()[..], b"up datagram");

        // The reply on the control stream and a datagram going down.
        control_tx.set_priority(0).unwrap();
        control_tx.write_all(b"welcome").await.unwrap();
        peer.send_datagram(Bytes::from_static(b"down datagram"))
            .unwrap();
        assert_eq!(
            next(&mut client).await,
            WsFrame::Data {
                stream: 0,
                payload: b"welcome"
            }
            .encode()
        );
        assert_eq!(
            next(&mut client).await,
            WsFrame::Datagram(b"down datagram").encode()
        );

        // A master-opened uni stream: bytes (split into chunks) then FIN.
        let mut uni = peer.open_uni().await.unwrap();
        uni.set_priority(-32).unwrap();
        let big = vec![7_u8; MAX_DATA_CHUNK + 10];
        uni.write_all(&big).await.unwrap();
        uni.finish().unwrap();
        assert_eq!(
            next(&mut client).await,
            WsFrame::OpenUni { stream: 1 }.encode()
        );
        let mut received = Vec::new();
        loop {
            let bytes = next(&mut client).await;
            match ws_frame::decode(&bytes).unwrap() {
                WsFrame::Data { stream: 1, payload } => received.extend_from_slice(payload),
                WsFrame::Fin { stream: 1 } => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(received, big);

        // FIN from the client ends the control reader.
        send(&mut client, WsFrame::Fin { stream: 0 }).await;
        assert!(control_rx.read_to_end(16).await.unwrap().is_empty());

        peer.close(0_u8.into(), b"session closed");
        assert_eq!(
            next(&mut client).await,
            WsFrame::Close {
                code: 0,
                reason: b"session closed"
            }
            .encode()
        );
    }

    #[tokio::test]
    async fn datagrams_drop_when_the_queue_is_deep() {
        let (peer, _client) = pair().await;
        // The client does not read, so TCP and the writer fill up.
        let payload = vec![1_u8; 1000];
        for _ in 0..200_000 {
            peer.send_datagram(&payload).unwrap();
            if peer.stats().dropped_out > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let stats = peer.stats();
        assert!(stats.dropped_out > 0, "no datagram was dropped");
        assert!(stats.queued_max <= DATAGRAM_QUEUE_MAX + 1);
    }

    #[tokio::test]
    async fn bootstrap_upload_is_read_to_end() {
        let (peer, mut client) = pair().await;
        send(&mut client, WsFrame::OpenBi { stream: 0 }).await;
        let (_tx, mut rx) = peer.accept_bi().await.unwrap();
        for part in [&b"abc"[..], b"defg"] {
            send(
                &mut client,
                WsFrame::Data {
                    stream: 0,
                    payload: part,
                },
            )
            .await;
        }
        send(&mut client, WsFrame::Fin { stream: 0 }).await;
        assert_eq!(rx.read_to_end(100).await.unwrap(), b"abcdefg");
    }
}
