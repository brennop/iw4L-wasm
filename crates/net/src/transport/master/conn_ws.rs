//! O16: browser WebSocket backend of the master connection, chosen with
//! `?transport=ws&master_ws=ws://host:port/` (the master's `--ws-bind`).
//! WebSocket uses Chrome's TCP path and none of its QUIC code, so a session on
//! it is the same-machine A/B for the WebTransport send-queue holds, and a
//! fallback for browsers without WebTransport.
//!
//! One WebSocket carries everything; each binary message is one frame of
//! `master_protocol::ws_frame` (datagrams, the control bidi stream, the
//! master's bootstrap uni streams). TCP gives back-pressure, so a stream write
//! never waits. A datagram is dropped, when `upstream_droppable` allows it,
//! while `bufferedAmount` is above `?ws_buf_max=` (default 16 KiB), so the
//! socket's queue never turns into seconds of stale upstream commands.
//!
//! Join only, like `conn_web.rs`: opening uni streams is refused. Everything
//! runs on the page's event loop, so nothing here is `Send`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use master_protocol::ws_frame::{self, MAX_DATA_CHUNK, WsFrame};
use tokio_util::sync::CancellationToken;
use wasm_bindgen::prelude::*;
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};

use super::conn::{ConnError, MasterConn, MasterRecvStream, MasterSendStream, hop_probe};
use super::conn_web::{conn_error, js_error};
use super::{MasterTarget, Result, rt, web_config};

const CONNECT_DEADLINE: Duration = Duration::from_secs(8);
/// `?ws_buf_max=` default: above this many buffered bytes a droppable
/// upstream datagram is dropped.
const DEFAULT_BUF_MAX: f64 = 16.0 * 1024.0;
/// Decoded datagrams waiting for the worker; the oldest is dropped beyond it.
const DATAGRAM_QUEUE_MAX: usize = 512;

#[derive(Default)]
struct RecvBuf {
    chunks: VecDeque<Vec<u8>>,
    fin: bool,
    waker: Option<Waker>,
}

#[derive(Default)]
struct State {
    open: bool,
    close: Option<String>,
    open_waker: Option<Waker>,
    datagrams: VecDeque<Vec<u8>>,
    datagram_waker: Option<Waker>,
    streams: HashMap<u32, RecvBuf>,
    uni_ready: VecDeque<u32>,
    uni_waker: Option<Waker>,
}

impl State {
    fn wake_all(&mut self) {
        for waker in [
            self.open_waker.take(),
            self.datagram_waker.take(),
            self.uni_waker.take(),
        ]
        .into_iter()
        .flatten()
        {
            waker.wake();
        }
        for buf in self.streams.values_mut() {
            if let Some(waker) = buf.waker.take() {
                waker.wake();
            }
        }
    }

    fn close_with(&mut self, reason: String) {
        self.close.get_or_insert(reason);
        self.wake_all();
    }
}

struct Inner {
    ws: WebSocket,
    state: Rc<RefCell<State>>,
    next_bi: Cell<u32>,
    buf_max: f64,
    dropped: Cell<u64>,
    written: Cell<u64>,
    max_buffered: Cell<f64>,
    queue_overflow: Rc<Cell<u64>>,
    counters_at: Cell<web_time::Instant>,
    priority_logged: Cell<bool>,
    on_open: Closure<dyn FnMut(Event)>,
    on_error: Closure<dyn FnMut(Event)>,
    on_close: Closure<dyn FnMut(CloseEvent)>,
    on_message: Closure<dyn FnMut(MessageEvent)>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.ws.set_onopen(None);
        self.ws.set_onerror(None);
        self.ws.set_onclose(None);
        self.ws.set_onmessage(None);
        let _ = self.ws.close();
        let _ = (
            &self.on_open,
            &self.on_error,
            &self.on_close,
            &self.on_message,
        );
    }
}

#[derive(Clone)]
pub(super) struct WsConn(Rc<Inner>);

pub(super) struct WsSend {
    inner: Rc<Inner>,
    id: u32,
}

pub(super) struct WsRecv {
    inner: Rc<Inner>,
    id: u32,
    chunk: Vec<u8>,
    offset: usize,
    done: bool,
}

/// `?ws_buf_max=` in bytes.
fn buf_max() -> f64 {
    web_config::query_param("ws_buf_max")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(DEFAULT_BUF_MAX)
}

/// Opens the WebSocket to `?master_ws=`. The first value stands in for
/// native's `quinn::Endpoint`, as in `conn_web::connect`.
pub(super) async fn connect(
    _target: &MasterTarget,
    cancel: &CancellationToken,
) -> Result<((), WsConn)> {
    let url = web_config::master_ws_url()
        .ok_or("transport=ws needs ?master_ws=ws://host:port/ (the master's --ws-bind)")?;
    let buf_max = buf_max();
    diag::info!(
        Net,
        "master websocket handshake begin url={url} ws_buf_max={buf_max}"
    );
    let started = web_time::Instant::now();
    let ws = WebSocket::new(&url)
        .map_err(|error| format!("new WebSocket({url}): {}", js_error(&error)))?;
    ws.set_binary_type(BinaryType::Arraybuffer);

    let state = Rc::new(RefCell::new(State::default()));
    let queue_overflow = Rc::new(Cell::new(0_u64));
    let on_open = {
        let state = Rc::clone(&state);
        Closure::<dyn FnMut(Event)>::new(move |_| {
            let mut state = state.borrow_mut();
            state.open = true;
            if let Some(waker) = state.open_waker.take() {
                waker.wake();
            }
        })
    };
    let on_error = {
        let state = Rc::clone(&state);
        Closure::<dyn FnMut(Event)>::new(move |_| {
            state.borrow_mut().close_with("websocket error".to_owned());
        })
    };
    let on_close = {
        let state = Rc::clone(&state);
        Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
            state.borrow_mut().close_with(format!(
                "closed code={} reason={:?} clean={}",
                event.code(),
                event.reason(),
                event.was_clean()
            ));
        })
    };
    let on_message = {
        let state = Rc::clone(&state);
        let overflow = Rc::clone(&queue_overflow);
        Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            on_message(&state, &overflow, &event);
        })
    };
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    let inner = Rc::new(Inner {
        ws,
        state: Rc::clone(&state),
        next_bi: Cell::new(0),
        buf_max,
        dropped: Cell::new(0),
        written: Cell::new(0),
        max_buffered: Cell::new(0.0),
        queue_overflow,
        counters_at: Cell::new(web_time::Instant::now()),
        priority_logged: Cell::new(false),
        on_open,
        on_error,
        on_close,
        on_message,
    });

    let opened = std::future::poll_fn(|cx| {
        let mut state = state.borrow_mut();
        if state.open {
            return Poll::Ready(Ok(()));
        }
        if let Some(reason) = &state.close {
            return Poll::Ready(Err(reason.clone()));
        }
        state.open_waker = Some(cx.waker().clone());
        Poll::Pending
    });
    let outcome: std::result::Result<(), String> = tokio::select! {
        _ = cancel.cancelled() => Err("session cancelled".into()),
        result = rt::timeout(CONNECT_DEADLINE, opened) => match result {
            Ok(result) => result,
            Err(_) => Err("master I/O deadline".into()),
        },
    };
    if let Err(error) = outcome {
        return Err(format!(
            "websocket handshake: {error}; url={url} elapsed_ms={}",
            started.elapsed().as_millis()
        )
        .into());
    }
    diag::info!(
        Net,
        "master websocket ready url={url} elapsed_ms={}",
        started.elapsed().as_millis()
    );
    Ok(((), WsConn(inner)))
}

/// Decodes one incoming message into the queues.
fn on_message(state: &Rc<RefCell<State>>, overflow: &Rc<Cell<u64>>, event: &MessageEvent) {
    let data = event.data();
    let Some(buffer) = data.dyn_ref::<js_sys::ArrayBuffer>() else {
        state
            .borrow_mut()
            .close_with("websocket: non-binary message".to_owned());
        return;
    };
    let bytes = js_sys::Uint8Array::new(buffer).to_vec();
    let frame = match ws_frame::decode(&bytes) {
        Ok(frame) => frame,
        Err(error) => {
            state
                .borrow_mut()
                .close_with(format!("websocket frame: {error}"));
            return;
        }
    };
    let mut state = state.borrow_mut();
    match frame {
        WsFrame::Datagram(payload) => {
            hop_probe::probe("B.recv", payload, "");
            if state.datagrams.len() >= DATAGRAM_QUEUE_MAX {
                state.datagrams.pop_front();
                overflow.set(overflow.get() + 1);
            }
            state.datagrams.push_back(payload.to_vec());
            if let Some(waker) = state.datagram_waker.take() {
                waker.wake();
            }
        }
        WsFrame::OpenUni { stream } => {
            state.streams.insert(stream, RecvBuf::default());
            state.uni_ready.push_back(stream);
            if let Some(waker) = state.uni_waker.take() {
                waker.wake();
            }
        }
        WsFrame::Data { stream, payload } => {
            if let Some(buf) = state.streams.get_mut(&stream) {
                buf.chunks.push_back(payload.to_vec());
                if let Some(waker) = buf.waker.take() {
                    waker.wake();
                }
            }
        }
        WsFrame::Fin { stream } => {
            if let Some(buf) = state.streams.get_mut(&stream) {
                buf.fin = true;
                if let Some(waker) = buf.waker.take() {
                    waker.wake();
                }
            }
        }
        WsFrame::Close { code, reason } => {
            state.close_with(format!(
                "closed by master code={code} reason={:?}",
                String::from_utf8_lossy(reason)
            ));
        }
        WsFrame::OpenBi { .. } | WsFrame::Stop { .. } => {}
    }
}

impl WsConn {
    fn send_frame(&self, frame: &WsFrame<'_>) -> std::result::Result<(), ConnError> {
        if let Some(reason) = &self.0.state.borrow().close {
            return Err(ConnError::from_display(format!(
                "websocket closed: {reason}"
            )));
        }
        self.0
            .ws
            .send_with_u8_array(&frame.encode())
            .map_err(conn_error)
    }

    /// `ws up counters:` under `pred_log`, every 5 s and at close.
    fn log_up_counters(&self, closing: bool) {
        if !crate::client::pred_log::enabled() {
            return;
        }
        let inner = &self.0;
        if !closing && inner.counters_at.get().elapsed() < Duration::from_secs(5) {
            return;
        }
        inner.counters_at.set(web_time::Instant::now());
        diag::info!(
            Net,
            "ws up counters: dropped={} written={} max_buffered={} down_queue_overflow={} buf_max={} closing={closing}",
            inner.dropped.get(),
            inner.written.get(),
            inner.max_buffered.get(),
            inner.queue_overflow.get(),
            inner.buf_max
        );
    }
}

impl MasterConn for WsConn {
    type SendStream = WsSend;
    type RecvStream = WsRecv;
    type Datagram = Vec<u8>;

    async fn open_bi(&self) -> std::result::Result<(WsSend, WsRecv), ConnError> {
        let id = self.0.next_bi.get();
        self.0.next_bi.set(id + 2);
        self.0
            .state
            .borrow_mut()
            .streams
            .insert(id, RecvBuf::default());
        self.send_frame(&WsFrame::OpenBi { stream: id })?;
        Ok((
            WsSend {
                inner: Rc::clone(&self.0),
                id,
            },
            WsRecv {
                inner: Rc::clone(&self.0),
                id,
                chunk: Vec::new(),
                offset: 0,
                done: false,
            },
        ))
    }

    async fn open_uni(&self) -> std::result::Result<WsSend, ConnError> {
        Err(ConnError::from_display(
            "the browser build joins only: hosting (bootstrap uni streams) is not supported",
        ))
    }

    async fn accept_uni(&self) -> std::result::Result<WsRecv, ConnError> {
        let id = std::future::poll_fn(|cx| {
            let mut state = self.0.state.borrow_mut();
            if let Some(id) = state.uni_ready.pop_front() {
                return Poll::Ready(Ok(id));
            }
            if let Some(reason) = &state.close {
                return Poll::Ready(Err(ConnError::from_display(format!(
                    "websocket closed: {reason}"
                ))));
            }
            state.uni_waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await?;
        Ok(WsRecv {
            inner: Rc::clone(&self.0),
            id,
            chunk: Vec::new(),
            offset: 0,
            done: false,
        })
    }

    fn send_datagram(&self, data: Vec<u8>) -> std::result::Result<(), ConnError> {
        let inner = &self.0;
        let buffered = f64::from(inner.ws.buffered_amount());
        if buffered > inner.max_buffered.get() {
            inner.max_buffered.set(buffered);
        }
        if buffered > inner.buf_max && hop_probe::upstream_droppable(&data) {
            inner.dropped.set(inner.dropped.get() + 1);
            hop_probe::probe("B.drop", &data, "");
            self.log_up_counters(false);
            return Ok(());
        }
        let extra = if crate::client::pred_log::enabled() {
            format!(" buffered={buffered}")
        } else {
            String::new()
        };
        hop_probe::probe("B.send", &data, &extra);
        self.send_frame(&WsFrame::Datagram(&data))?;
        inner.written.set(inner.written.get() + 1);
        self.log_up_counters(false);
        Ok(())
    }

    async fn read_datagram(&self) -> std::result::Result<Vec<u8>, ConnError> {
        std::future::poll_fn(|cx| {
            let mut state = self.0.state.borrow_mut();
            if let Some(datagram) = state.datagrams.pop_front() {
                return Poll::Ready(Ok(datagram));
            }
            if let Some(reason) = &state.close {
                return Poll::Ready(Err(ConnError::from_display(format!(
                    "websocket closed: {reason}"
                ))));
            }
            state.datagram_waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    fn close(&self, code: u32, reason: &[u8]) {
        self.log_up_counters(true);
        let _ = self.send_frame(&WsFrame::Close { code, reason });
        let _ = self.0.ws.close();
        self.0.state.borrow_mut().close_with(format!(
            "closed locally code={code} reason={:?}",
            String::from_utf8_lossy(reason)
        ));
    }

    fn close_reason(&self) -> Option<String> {
        self.0.state.borrow().close.clone()
    }
}

impl MasterSendStream for WsSend {
    fn set_priority(&self, _priority: i32) -> std::result::Result<(), ConnError> {
        // The browser only sends the small control stream, so there is no
        // priority to keep between streams (the master orders its own sends).
        if !self.inner.priority_logged.replace(true) {
            diag::info!(Net, "ws: stream priority is ignored in the browser");
        }
        Ok(())
    }

    async fn write_all(&mut self, bytes: &[u8]) -> std::result::Result<(), ConnError> {
        let conn = WsConn(Rc::clone(&self.inner));
        for chunk in bytes.chunks(MAX_DATA_CHUNK) {
            conn.send_frame(&WsFrame::Data {
                stream: self.id,
                payload: chunk,
            })?;
        }
        Ok(())
    }

    fn finish(&mut self) -> std::result::Result<(), ConnError> {
        WsConn(Rc::clone(&self.inner)).send_frame(&WsFrame::Fin { stream: self.id })
    }
}

impl WsRecv {
    /// Refills `chunk`; false at the end of the stream.
    async fn fill(&mut self) -> std::result::Result<bool, ConnError> {
        while self.offset >= self.chunk.len() {
            if self.done {
                return Ok(false);
            }
            let next = std::future::poll_fn(|cx| {
                let mut state = self.inner.state.borrow_mut();
                let closed = state.close.clone();
                let Some(buf) = state.streams.get_mut(&self.id) else {
                    return Poll::Ready(Ok(None));
                };
                if let Some(chunk) = buf.chunks.pop_front() {
                    return Poll::Ready(Ok(Some(chunk)));
                }
                if buf.fin {
                    return Poll::Ready(Ok(None));
                }
                if let Some(reason) = closed {
                    return Poll::Ready(Err(ConnError::from_display(format!(
                        "websocket closed: {reason}"
                    ))));
                }
                buf.waker = Some(cx.waker().clone());
                Poll::Pending
            })
            .await?;
            match next {
                Some(chunk) => {
                    self.chunk = chunk;
                    self.offset = 0;
                }
                None => self.done = true,
            }
        }
        Ok(true)
    }
}

impl MasterRecvStream for WsRecv {
    async fn read_exact(&mut self, buf: &mut [u8]) -> std::result::Result<(), ConnError> {
        let mut filled = 0;
        while filled < buf.len() {
            if !self.fill().await? {
                return Err(ConnError::from_display(format!(
                    "stream finished early ({filled} of {} bytes)",
                    buf.len()
                )));
            }
            let take = (buf.len() - filled).min(self.chunk.len() - self.offset);
            buf[filled..filled + take]
                .copy_from_slice(&self.chunk[self.offset..self.offset + take]);
            filled += take;
            self.offset += take;
        }
        Ok(())
    }

    async fn read_to_end(&mut self, limit: usize) -> std::result::Result<Vec<u8>, ConnError> {
        let mut out = Vec::new();
        while self.fill().await? {
            out.extend_from_slice(&self.chunk[self.offset..]);
            self.offset = self.chunk.len();
            if out.len() > limit {
                return Err(ConnError::from_display(format!(
                    "stream longer than {limit} bytes"
                )));
            }
        }
        Ok(out)
    }
}
