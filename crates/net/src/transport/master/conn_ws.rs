//! O16: browser WebSocket backend of the master connection, chosen with
//! `?transport=ws&master_ws=ws://host:port/` (the master's `--ws-bind`). Since
//! O19 it is the fallback: the default is `wtw` below, and ws is used when the
//! URL says so (`master_ws=` alone, `transport=ws`) or the master serving the
//! page has no WebTransport listener.
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
//! O19: `WsConn` now carries two pipes (`Pipe`) with the same frames. With
//! `?transport=wtw` the pipe is a dedicated worker (`iw4l-wt-worker.js`) that
//! owns a real WebTransport session and maps the frames onto it, so a busy
//! main thread cannot hold back Chrome's datagram writable (O15, O18). The
//! worker's `ready` message stands in for the socket's `onopen`; page -> worker
//! messages carry a one-byte envelope prefix (`hop_probe::worker_envelope`),
//! worker -> page messages are bare frames, and control messages are plain
//! objects (see the worker's header).
//!
//! Join only, like `conn_web.rs`: opening uni streams is refused. Everything
//! runs on the page's event loop, so nothing here is `Send`.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use js_sys::{Array, Object, Reflect, Uint8Array};
use master_protocol::ws_frame::{self, MAX_DATA_CHUNK, WsFrame};
use tokio_util::sync::CancellationToken;
use wasm_bindgen::prelude::*;
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};

use super::conn::{ConnError, MasterConn, MasterRecvStream, MasterSendStream, hop_probe};
use super::conn_web::{WtParams, conn_error, get, js_error, set};
use super::{MasterTarget, Result, rt, web_config};

// The worker is declared here, not taken from `web-sys`, like the WebTransport
// bindings in `conn_web.rs` (no extra `web-sys` feature).
#[wasm_bindgen]
extern "C" {
    type JsWorker;
    #[wasm_bindgen(catch, constructor, js_class = "Worker")]
    fn new(url: &str) -> std::result::Result<JsWorker, JsValue>;
    #[wasm_bindgen(method, catch, js_name = postMessage)]
    fn post_message(this: &JsWorker, message: &JsValue) -> std::result::Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = postMessage)]
    fn post_message_transfer(
        this: &JsWorker,
        message: &JsValue,
        transfer: &JsValue,
    ) -> std::result::Result<(), JsValue>;
    #[wasm_bindgen(method)]
    fn terminate(this: &JsWorker);
    #[wasm_bindgen(method, setter, js_name = onmessage)]
    fn set_onmessage(this: &JsWorker, handler: Option<&js_sys::Function>);
    #[wasm_bindgen(method, setter, js_name = onerror)]
    fn set_onerror(this: &JsWorker, handler: Option<&js_sys::Function>);
}

/// What carries the frames.
enum Pipe {
    Socket(WebSocket),
    Worker(JsWorker),
}

/// How long a worker lives after its close frame is posted, so the
/// WebTransport close reaches the network before the worker is terminated.
const WORKER_TERMINATE_DELAY_MS: i32 = 1000;

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
    /// Worker pipe: the `ready` message's details, for the log.
    worker_ready: String,
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
    pipe: Pipe,
    state: Rc<RefCell<State>>,
    next_bi: Cell<u32>,
    buf_max: f64,
    /// Worker pipe: `?up_drop=bp` is on, so datagrams carry the droppable bit.
    drop_bp: bool,
    dropped: Cell<u64>,
    written: Cell<u64>,
    max_buffered: Cell<f64>,
    queue_overflow: Rc<Cell<u64>>,
    counters_at: Cell<web_time::Instant>,
    priority_logged: Cell<bool>,
    /// Worker pipe: the close frame was posted.
    close_posted: Cell<bool>,
    /// Worker pipe: `terminate()` was called.
    terminated: Cell<bool>,
    /// The JS handlers, alive as long as the pipe.
    _handlers: Vec<Box<dyn Any>>,
}

impl Inner {
    /// Worker pipe: stops the worker now.
    fn terminate_worker(&self) {
        if let Pipe::Worker(worker) = &self.pipe
            && !self.terminated.replace(true)
        {
            worker.terminate();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        match &self.pipe {
            Pipe::Socket(ws) => {
                ws.set_onopen(None);
                ws.set_onerror(None);
                ws.set_onclose(None);
                ws.set_onmessage(None);
                let _ = ws.close();
            }
            Pipe::Worker(worker) => {
                if self.terminated.get() {
                    worker.set_onmessage(None);
                    worker.set_onerror(None);
                    return;
                }
                if !self.close_posted.replace(true) {
                    let close = WsFrame::Close {
                        code: 0,
                        reason: b"connection dropped",
                    };
                    let _ = post_to_worker(worker, &close.encode(), false, None);
                }
                // Terminate once the close has had time to reach the network;
                // the worker's message handlers are not needed by then.
                worker.set_onmessage(None);
                worker.set_onerror(None);
                let worker: &JsValue = worker;
                let worker = worker.clone().unchecked_into::<JsWorker>();
                let terminate = Closure::once_into_js(move || worker.terminate());
                if let Some(window) = web_sys::window() {
                    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                        terminate.unchecked_ref(),
                        WORKER_TERMINATE_DELAY_MS,
                    );
                }
            }
        }
    }
}

/// Posts one frame to the worker, in its envelope, as a transferred buffer.
fn post_to_worker(
    worker: &JsWorker,
    frame: &[u8],
    droppable: bool,
    probe_hash: Option<u64>,
) -> std::result::Result<(), ConnError> {
    let envelope = hop_probe::worker_envelope(frame, droppable, probe_hash);
    let buffer = Uint8Array::from(&envelope[..]).buffer();
    worker
        .post_message_transfer(&buffer, &Array::of1(&buffer))
        .map_err(conn_error)
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

/// Waits for the pipe to open (the socket's `onopen`, or the worker's `ready`),
/// for the close reason, for cancellation or for the connect deadline.
async fn wait_open(
    state: &Rc<RefCell<State>>,
    cancel: &CancellationToken,
) -> std::result::Result<(), String> {
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
    tokio::select! {
        _ = cancel.cancelled() => Err("session cancelled".into()),
        result = rt::timeout(CONNECT_DEADLINE, opened) => match result {
            Ok(result) => result,
            Err(_) => Err("master I/O deadline".into()),
        },
    }
}

fn new_inner(
    pipe: Pipe,
    state: &Rc<RefCell<State>>,
    queue_overflow: Rc<Cell<u64>>,
    buf_max: f64,
    drop_bp: bool,
    handlers: Vec<Box<dyn Any>>,
) -> Rc<Inner> {
    Rc::new(Inner {
        pipe,
        state: Rc::clone(state),
        next_bi: Cell::new(0),
        buf_max,
        drop_bp,
        dropped: Cell::new(0),
        written: Cell::new(0),
        max_buffered: Cell::new(0.0),
        queue_overflow,
        counters_at: Cell::new(web_time::Instant::now()),
        priority_logged: Cell::new(false),
        close_posted: Cell::new(false),
        terminated: Cell::new(false),
        _handlers: handlers,
    })
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
            on_message(&state, &overflow, &event, false);
        })
    };
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    let inner = new_inner(
        Pipe::Socket(ws),
        &state,
        queue_overflow,
        buf_max,
        false,
        vec![
            Box::new(on_open),
            Box::new(on_error),
            Box::new(on_close),
            Box::new(on_message),
        ],
    );

    if let Err(error) = wait_open(&state, cancel).await {
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

/// O19 `?transport=wtw`: starts the WebTransport worker and has it open the
/// session to `target`, with the same URL, certificate hash and `?wt_cc=` as
/// `conn_web::connect_wt`. "Open" is the worker's `ready` message.
pub(super) async fn connect_worker(
    target: &MasterTarget,
    cancel: &CancellationToken,
) -> Result<((), WsConn)> {
    if !target.ca_pem.is_empty() {
        diag::warn!(
            Net,
            "master CA ignored: a browser trusts a certificate hash or the Web PKI"
        );
    }
    let params = WtParams::from_query()?;
    let worker_url = worker_url();
    diag::info!(
        Net,
        "master wtw handshake begin url={} worker={worker_url} trust={}",
        target.address,
        if params.hash.is_some() {
            "certificate-hash"
        } else {
            "web-pki"
        }
    );
    let started = web_time::Instant::now();
    let worker = JsWorker::new(&worker_url)
        .map_err(|error| format!("new Worker({worker_url}): {}", js_error(&error)))?;

    let state = Rc::new(RefCell::new(State::default()));
    let queue_overflow = Rc::new(Cell::new(0_u64));
    let on_message = {
        let state = Rc::clone(&state);
        let overflow = Rc::clone(&queue_overflow);
        Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            on_message(&state, &overflow, &event, true);
        })
    };
    // A worker whose script fails to load or throws reports through `error`.
    let on_error = {
        let state = Rc::clone(&state);
        Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            let detail = |key: &str| {
                get(&event, key)
                    .as_string()
                    .unwrap_or_else(|| "".to_owned())
            };
            state.borrow_mut().close_with(format!(
                "worker error: {} ({}:{})",
                detail("message"),
                detail("filename"),
                get(&event, "lineno").as_f64().unwrap_or(0.0)
            ));
        })
    };
    worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    let (up_drop, _, _) = super::conn_web::up_drop_mode();
    let drop_bp = matches!(up_drop, "bp" | "both");
    let bp_thresh = web_config::query_param("up_bp_thresh")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(-1.0);
    let options = Object::new();
    if let Some(cc) = &params.cc {
        set(&options, "congestionControl", &JsValue::from_str(cc));
    }
    set(
        &options,
        "upDrop",
        &JsValue::from_str(if drop_bp { "bp" } else { "off" }),
    );
    set(&options, "bpThresh", &JsValue::from_f64(bp_thresh));
    let connect = Object::new();
    set(&connect, "type", &JsValue::from_str("connect"));
    set(&connect, "url", &JsValue::from_str(&target.address));
    set(
        &connect,
        "certHash",
        &params
            .hash
            .map_or(JsValue::NULL, |hash| Uint8Array::from(&hash[..]).into()),
    );
    set(&connect, "options", &options);
    let inner = new_inner(
        Pipe::Worker(worker),
        &state,
        queue_overflow,
        0.0,
        drop_bp,
        vec![Box::new(on_message), Box::new(on_error)],
    );
    let Pipe::Worker(worker) = &inner.pipe else {
        unreachable!("just built as a worker pipe");
    };
    if let Err(error) = worker.post_message(&connect) {
        inner.terminate_worker();
        return Err(format!("worker connect message: {}", js_error(&error)).into());
    }
    diag::info!(
        Net,
        "wtw up_drop mode={} bp_thresh={bp_thresh}",
        if drop_bp { "bp" } else { "off" }
    );

    if let Err(error) = wait_open(&state, cancel).await {
        inner.terminate_worker();
        return Err(format!(
            "wtw handshake: {error}; url={} elapsed_ms={}",
            target.address,
            started.elapsed().as_millis()
        )
        .into());
    }
    diag::info!(
        Net,
        "master wtw ready url={} {} elapsed_ms={}",
        target.address,
        state.borrow().worker_ready,
        started.elapsed().as_millis()
    );
    Ok(((), WsConn(inner)))
}

/// `window.IW4L_WT_WORKER` (set by `index.html`, versioned by `xtask web`), or
/// the unversioned name.
fn worker_url() -> String {
    web_sys::window()
        .and_then(|window| Reflect::get(&window, &JsValue::from_str("IW4L_WT_WORKER")).ok())
        .and_then(|value| value.as_string())
        .unwrap_or_else(|| "./iw4l-wt-worker.js".to_owned())
}

/// One message from the pipe: a frame (an `ArrayBuffer`), or, from the worker,
/// a control object.
fn on_message(
    state: &Rc<RefCell<State>>,
    overflow: &Rc<Cell<u64>>,
    event: &MessageEvent,
    from_worker: bool,
) {
    let data = event.data();
    if let Some(buffer) = data.dyn_ref::<js_sys::ArrayBuffer>() {
        on_frame(state, overflow, &Uint8Array::new(buffer).to_vec());
    } else if from_worker && data.is_object() {
        on_control(state, &data);
    } else {
        state
            .borrow_mut()
            .close_with("websocket: non-binary message".to_owned());
    }
}

/// Worker control message (`ready`, `error`, `probe`, `stats`).
fn on_control(state: &Rc<RefCell<State>>, message: &JsValue) {
    let text = |key: &str| {
        get(message, key)
            .as_string()
            .unwrap_or_else(|| "".to_owned())
    };
    let number = |key: &str| get(message, key).as_f64();
    match text("type").as_str() {
        "ready" => {
            let mut state = state.borrow_mut();
            state.worker_ready = format!(
                "max_datagram_size={} congestion_control={:?}",
                number("maxDatagramSize").unwrap_or(0.0),
                get(message, "congestionControl")
            );
            state.open = true;
            if let Some(waker) = state.open_waker.take() {
                waker.wake();
            }
        }
        "error" => state
            .borrow_mut()
            .close_with(format!("worker: {}", text("message"))),
        // O13 `hop probe` line of the datagram write, from the worker's clock
        // (the same Unix clock) so `hops.py` reads it as the page's B.write.
        "probe" => {
            let desired = number("desired_size");
            diag::info!(
                Net,
                "hop probe: hop=B.write dir=up hash={} len={} unix_ms={} write_ms={} desired_size={desired:?}",
                text("hash"),
                number("len").unwrap_or(0.0),
                number("unix_ms").unwrap_or(0.0),
                number("write_ms").unwrap_or(0.0)
            );
        }
        "stats" => {
            if crate::client::pred_log::enabled() {
                diag::info!(
                    Net,
                    "wtw up counters: dropped={} written={} min_desired_size={} oversize={}",
                    number("dropped").unwrap_or(0.0),
                    number("written").unwrap_or(0.0),
                    number("min_desired").map_or_else(|| "none".to_owned(), |d| d.to_string()),
                    number("oversize").unwrap_or(0.0)
                );
            }
        }
        other => diag::warn!(Net, "wtw: unknown worker message type {other:?}"),
    }
}

/// Decodes one frame into the queues.
fn on_frame(state: &Rc<RefCell<State>>, overflow: &Rc<Cell<u64>>, bytes: &[u8]) {
    let frame = match ws_frame::decode(bytes) {
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
        self.send_frame_with(frame, false, None)
    }

    /// `droppable` and `probe_hash` go in the worker pipe's envelope; the
    /// socket pipe has no use for them.
    fn send_frame_with(
        &self,
        frame: &WsFrame<'_>,
        droppable: bool,
        probe_hash: Option<u64>,
    ) -> std::result::Result<(), ConnError> {
        if let Some(reason) = &self.0.state.borrow().close {
            return Err(ConnError::from_display(format!(
                "websocket closed: {reason}"
            )));
        }
        match &self.0.pipe {
            Pipe::Socket(ws) => ws.send_with_u8_array(&frame.encode()).map_err(conn_error),
            Pipe::Worker(worker) => post_to_worker(worker, &frame.encode(), droppable, probe_hash),
        }
    }

    /// `ws up counters:` under `pred_log`, every 5 s and at close. (The worker
    /// pipe's counters are the worker's `stats` messages.)
    fn log_up_counters(&self, closing: bool) {
        if !crate::client::pred_log::enabled() || matches!(self.0.pipe, Pipe::Worker(_)) {
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
        let ws = match &inner.pipe {
            Pipe::Socket(ws) => ws,
            Pipe::Worker(_) => {
                // No `bufferedAmount` here; the worker drops on `desiredSize`
                // (`?up_drop=bp`) and counts it in its `stats` messages.
                let droppable = inner.drop_bp && hop_probe::upstream_droppable(&data);
                let hash = hop_probe::probe("B.send", &data, "");
                self.send_frame_with(&WsFrame::Datagram(&data), droppable, hash)?;
                inner.written.set(inner.written.get() + 1);
                return Ok(());
            }
        };
        let buffered = f64::from(ws.buffered_amount());
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
        if !self.0.close_posted.replace(true) {
            let _ = self.send_frame(&WsFrame::Close { code, reason });
        }
        // The worker pipe stays up until `Inner` drops, so its WebTransport
        // close can reach the network (`WORKER_TERMINATE_DELAY_MS`).
        if let Pipe::Socket(ws) = &self.0.pipe {
            let _ = ws.close();
        }
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
