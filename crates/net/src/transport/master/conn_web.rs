//! Browser backend of the master connection: one WebTransport session to the
//! master's WebTransport listener (`iw4l-master --webtransport-bind`). Join
//! only: the control bidi stream, datagrams both ways and incoming bootstrap
//! uni streams; opening uni streams (the host's bootstrap send) is refused.
//!
//! The WebTransport bindings are declared here rather than taken from
//! `web-sys`, whose WebTransport API is behind `--cfg=web_sys_unstable_apis`
//! (a rustflag that would apply to every crate of the wasm build); the few
//! members used are stable in Chrome. Streams use `web-sys`'s stable
//! `ReadableStream`/`WritableStream` types. Everything runs on the page's
//! event loop (`rt_web.rs`), so nothing here is `Send`.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

use js_sys::{Array, Object, Reflect, Uint8Array};
use tokio_util::sync::CancellationToken;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    ReadableStream, ReadableStreamDefaultReader, WritableStream, WritableStreamDefaultWriter,
};

use super::conn::{ConnError, MasterConn, MasterRecvStream, MasterSendStream, hop_probe};
use super::{MasterTarget, Result, rt, web_config};

const CONNECT_DEADLINE: Duration = Duration::from_secs(8);

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = WebTransport)]
    type JsWebTransport;
    #[wasm_bindgen(catch, constructor, js_class = "WebTransport")]
    fn new(url: &str, options: &Object) -> std::result::Result<JsWebTransport, JsValue>;
    #[wasm_bindgen(method, getter)]
    fn ready(this: &JsWebTransport) -> js_sys::Promise;
    #[wasm_bindgen(method, getter)]
    fn closed(this: &JsWebTransport) -> js_sys::Promise;
    #[wasm_bindgen(method, getter)]
    fn datagrams(this: &JsWebTransport) -> JsDatagrams;
    #[wasm_bindgen(method, getter, js_name = incomingUnidirectionalStreams)]
    fn incoming_unidirectional_streams(this: &JsWebTransport) -> ReadableStream;
    #[wasm_bindgen(method, js_name = createBidirectionalStream)]
    fn create_bidirectional_stream(this: &JsWebTransport) -> js_sys::Promise;
    #[wasm_bindgen(method)]
    fn close(this: &JsWebTransport, info: &Object);

    type JsDatagrams;
    #[wasm_bindgen(method, getter)]
    fn readable(this: &JsDatagrams) -> ReadableStream;
    #[wasm_bindgen(method, getter)]
    fn writable(this: &JsDatagrams) -> WritableStream;
    #[wasm_bindgen(method, getter, js_name = maxDatagramSize)]
    fn max_datagram_size(this: &JsDatagrams) -> f64;
}

fn js_error(value: &JsValue) -> String {
    if let Some(error) = value.dyn_ref::<js_sys::Error>() {
        return format!(
            "{}: {}",
            String::from(error.name()),
            String::from(error.message())
        );
    }
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}

fn conn_error(value: JsValue) -> ConnError {
    ConnError::from_display(js_error(&value))
}

fn get(object: &JsValue, key: &str) -> JsValue {
    Reflect::get(object, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

fn set(object: &JsValue, key: &str, value: &JsValue) {
    let _ = Reflect::set(object, &JsValue::from_str(key), value);
}

/// Awaits `promise` in the background so a rejection is handled.
fn forget(promise: js_sys::Promise) {
    spawn_local(async move {
        let _ = JsFuture::from(promise).await;
    });
}

fn default_reader(stream: &ReadableStream) -> ReadableStreamDefaultReader {
    stream.get_reader().unchecked_into()
}

/// One `reader.read()` that survives being dropped: a cancelled `read` leaves
/// its promise here and the next call resumes it, so no chunk is lost when a
/// `select!` drops the future.
#[derive(Default)]
struct PendingRead(RefCell<Option<JsFuture>>);

impl PendingRead {
    /// The next chunk, or `None` when the stream is done.
    async fn read(
        &self,
        reader: &ReadableStreamDefaultReader,
    ) -> std::result::Result<Option<JsValue>, ConnError> {
        let result = std::future::poll_fn(|cx| {
            let mut slot = self.0.borrow_mut();
            let read = slot.get_or_insert_with(|| JsFuture::from(reader.read()));
            match Pin::new(read).poll(cx) {
                Poll::Ready(result) => {
                    *slot = None;
                    Poll::Ready(result)
                }
                Poll::Pending => Poll::Pending,
            }
        })
        .await
        .map_err(conn_error)?;
        if get(&result, "done").as_bool().unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(get(&result, "value")))
    }
}

struct Inner {
    transport: JsWebTransport,
    datagrams: JsDatagrams,
    datagram_reader: ReadableStreamDefaultReader,
    datagram_writer: WritableStreamDefaultWriter,
    datagram_read: PendingRead,
    uni_reader: ReadableStreamDefaultReader,
    uni_read: PendingRead,
    close_reason: Rc<RefCell<Option<String>>>,
    oversize_drops: Cell<u64>,
}

#[derive(Clone)]
pub(super) struct WebConn(Rc<Inner>);

pub(super) struct WebSend {
    stream: JsValue,
    writer: WritableStreamDefaultWriter,
}

pub(super) struct WebRecv {
    reader: ReadableStreamDefaultReader,
    read: PendingRead,
    chunk: Vec<u8>,
    offset: usize,
    done: bool,
}

impl WebRecv {
    fn new(stream: &ReadableStream) -> Self {
        Self {
            reader: default_reader(stream),
            read: PendingRead::default(),
            chunk: Vec::new(),
            offset: 0,
            done: false,
        }
    }

    /// Refills `chunk`; false at the end of the stream.
    async fn fill(&mut self) -> std::result::Result<bool, ConnError> {
        while self.offset >= self.chunk.len() {
            if self.done {
                return Ok(false);
            }
            match self.read.read(&self.reader).await? {
                Some(value) => {
                    self.chunk = value.unchecked_into::<Uint8Array>().to_vec();
                    self.offset = 0;
                }
                None => self.done = true,
            }
        }
        Ok(true)
    }
}

/// Opens the WebTransport session. The certificate hash comes from the page
/// URL for now (`web_config.rs`, O5 moves it into `MasterTarget`). The first
/// value stands in for native's `quinn::Endpoint`.
pub(super) async fn connect(
    target: &MasterTarget,
    cancel: &CancellationToken,
) -> Result<((), WebConn)> {
    if let Some(path) = &target.ca_cert {
        diag::warn!(
            Net,
            "master CA file {} ignored: a browser trusts a certificate hash or the Web PKI",
            path.display()
        );
    }
    let hash = web_config::cert_hash()?;
    let options = Object::new();
    if let Some(hash) = hash {
        let entry = Object::new();
        set(&entry, "algorithm", &JsValue::from_str("sha-256"));
        set(&entry, "value", &Uint8Array::from(&hash[..]));
        set(&options, "serverCertificateHashes", &Array::of1(&entry));
    }
    diag::info!(
        Net,
        "master webtransport handshake begin url={} trust={}",
        target.address,
        if hash.is_some() {
            "certificate-hash"
        } else {
            "web-pki"
        }
    );
    let started = web_time::Instant::now();
    let transport = JsWebTransport::new(&target.address, &options)
        .map_err(|error| format!("new WebTransport({}): {}", target.address, js_error(&error)))?;

    let close_reason = Rc::new(RefCell::new(None));
    {
        let close_reason = Rc::clone(&close_reason);
        let closed = JsFuture::from(transport.closed());
        spawn_local(async move {
            let reason = match closed.await {
                Ok(info) => format!(
                    "closed code={} reason={:?}",
                    get(&info, "closeCode").as_f64().unwrap_or(0.0),
                    get(&info, "reason")
                        .as_string()
                        .unwrap_or_else(|| "".to_owned())
                ),
                Err(error) => js_error(&error),
            };
            close_reason.borrow_mut().get_or_insert(reason);
        });
    }

    let ready = JsFuture::from(transport.ready());
    let outcome: std::result::Result<(), String> = tokio::select! {
        _ = cancel.cancelled() => Err("session cancelled".into()),
        result = rt::timeout(CONNECT_DEADLINE, ready) => match result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(js_error(&error)),
            Err(_) => Err("master I/O deadline".into()),
        },
    };
    if let Err(error) = outcome {
        transport.close(&Object::new());
        return Err(format!(
            "webtransport handshake: {error}; url={} elapsed_ms={}",
            target.address,
            started.elapsed().as_millis()
        )
        .into());
    }

    let datagrams = transport.datagrams();
    let datagram_writer = datagrams
        .writable()
        .get_writer()
        .map_err(|error| format!("datagram writer: {}", js_error(&error)))?;
    let inner = Inner {
        datagram_reader: default_reader(&datagrams.readable()),
        datagram_writer,
        datagram_read: PendingRead::default(),
        uni_reader: default_reader(&transport.incoming_unidirectional_streams()),
        uni_read: PendingRead::default(),
        datagrams,
        transport,
        close_reason,
        oversize_drops: Cell::new(0),
    };
    diag::info!(
        Net,
        "master webtransport ready url={} max_datagram_size={} elapsed_ms={}",
        target.address,
        inner.datagrams.max_datagram_size(),
        started.elapsed().as_millis()
    );
    if crate::client::pred_log::enabled() {
        log_datagram_queues(&inner.datagrams);
        start_stats_loop(&inner.transport, Rc::clone(&inner.close_reason));
    }
    Ok(((), WebConn(Rc::new(inner))))
}

/// O13: the datagram queue settings, once at connect.
fn log_datagram_queues(datagrams: &JsDatagrams) {
    let value = |key: &str| format!("{:?}", get(datagrams, key));
    diag::info!(
        Net,
        "wt datagram queues: incomingHighWaterMark={} incomingMaxAge={} outgoingHighWaterMark={} outgoingMaxAge={}",
        value("incomingHighWaterMark"),
        value("incomingMaxAge"),
        value("outgoingHighWaterMark"),
        value("outgoingMaxAge")
    );
}

/// Appends every numeric field of `value` to `out` as `path=number`, nested
/// objects as `outer.inner`.
fn flatten_numbers(value: &JsValue, path: &str, out: &mut Vec<String>) {
    if let Some(number) = value.as_f64() {
        out.push(format!("{path}={number}"));
    } else if value.is_object() {
        for key in Object::keys(value.unchecked_ref::<Object>()).iter() {
            let Some(key) = key.as_string() else { continue };
            let child = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            flatten_numbers(&get(value, &key), &child, out);
        }
    }
}

/// O13: `WebTransport.getStats()` every 5 s until the session closes.
fn start_stats_loop(transport: &JsWebTransport, close_reason: Rc<RefCell<Option<String>>>) {
    let transport: &JsValue = transport;
    let transport = transport.clone();
    let get_stats = get(&transport, "getStats");
    let Some(get_stats) = get_stats.dyn_ref::<js_sys::Function>().cloned() else {
        diag::info!(Net, "wt stats: WebTransport.getStats is not available");
        return;
    };
    spawn_local(async move {
        while close_reason.borrow().is_none() {
            rt::sleep(Duration::from_secs(5)).await;
            let Ok(promise) = get_stats.call0(&transport) else {
                return;
            };
            let Ok(stats) = JsFuture::from(js_sys::Promise::from(promise)).await else {
                continue;
            };
            let mut fields = Vec::new();
            flatten_numbers(&stats, "", &mut fields);
            diag::info!(Net, "wt stats: {}", fields.join(" "));
        }
    });
}

impl MasterConn for WebConn {
    type SendStream = WebSend;
    type RecvStream = WebRecv;
    type Datagram = Vec<u8>;

    async fn open_bi(&self) -> std::result::Result<(WebSend, WebRecv), ConnError> {
        let stream = JsFuture::from(self.0.transport.create_bidirectional_stream())
            .await
            .map_err(conn_error)?;
        let readable: ReadableStream = get(&stream, "readable").unchecked_into();
        let writable: WritableStream = get(&stream, "writable").unchecked_into();
        let writer = writable.get_writer().map_err(conn_error)?;
        Ok((
            WebSend {
                stream: writable.into(),
                writer,
            },
            WebRecv::new(&readable),
        ))
    }

    async fn open_uni(&self) -> std::result::Result<WebSend, ConnError> {
        Err(ConnError::from_display(
            "the browser build joins only: hosting (bootstrap uni streams) is not supported",
        ))
    }

    async fn accept_uni(&self) -> std::result::Result<WebRecv, ConnError> {
        match self.0.uni_read.read(&self.0.uni_reader).await? {
            Some(stream) => Ok(WebRecv::new(&stream.unchecked_into())),
            None => Err(ConnError::from_display("connection closed")),
        }
    }

    fn send_datagram(&self, data: Vec<u8>) -> std::result::Result<(), ConnError> {
        // Chrome reports 1024 where quinn allows 1413, below the relay's
        // 1118-byte wire maximum. A smaller fragment size is no fix (the
        // host's reassembler expects its own), so an oversize datagram is
        // dropped like a lost one instead of failing the session.
        let max = self.0.datagrams.max_datagram_size();
        if data.len() as f64 > max {
            let dropped = self.0.oversize_drops.get() + 1;
            self.0.oversize_drops.set(dropped);
            if dropped == 1 || dropped.is_multiple_of(100) {
                diag::warn!(
                    Net,
                    "master datagram of {} bytes dropped: browser maxDatagramSize is {max} ({dropped} dropped so far)",
                    data.len()
                );
            }
            return Ok(());
        }
        let hash = hop_probe::probe("B.send", &data, "");
        let desired = hash.and_then(|_| self.0.datagram_writer.desired_size().ok().flatten());
        let chunk = Uint8Array::from(&data[..]);
        let promise = self.0.datagram_writer.write_with_chunk(&chunk);
        match hash {
            Some(hash) => {
                let len = data.len();
                spawn_local(async move {
                    let started = web_time::Instant::now();
                    let _ = JsFuture::from(promise).await;
                    diag::info!(
                        Net,
                        "hop probe: hop=B.write dir=up hash={hash:016x} len={len} unix_ms={} write_ms={} desired_size={desired:?}",
                        hop_probe::unix_ms(),
                        started.elapsed().as_millis()
                    );
                });
            }
            None => forget(promise),
        }
        Ok(())
    }

    async fn read_datagram(&self) -> std::result::Result<Vec<u8>, ConnError> {
        match self.0.datagram_read.read(&self.0.datagram_reader).await? {
            Some(value) => {
                let datagram = value.unchecked_into::<Uint8Array>().to_vec();
                hop_probe::probe("B.recv", &datagram, "");
                Ok(datagram)
            }
            None => Err(ConnError::from_display("connection closed")),
        }
    }

    fn close(&self, code: u32, reason: &[u8]) {
        let reason = String::from_utf8_lossy(reason).into_owned();
        let info = Object::new();
        set(&info, "closeCode", &JsValue::from_f64(f64::from(code)));
        set(&info, "reason", &JsValue::from_str(&reason));
        self.0.transport.close(&info);
        self.0
            .close_reason
            .borrow_mut()
            .get_or_insert_with(|| format!("closed locally code={code} reason={reason:?}"));
    }

    fn close_reason(&self) -> Option<String> {
        self.0.close_reason.borrow().clone()
    }
}

impl MasterSendStream for WebSend {
    fn set_priority(&self, priority: i32) -> std::result::Result<(), ConnError> {
        // WebTransport `sendOrder`: higher is sent first, as with quinn.
        set(
            &self.stream,
            "sendOrder",
            &JsValue::from_f64(f64::from(priority)),
        );
        Ok(())
    }

    async fn write_all(&mut self, bytes: &[u8]) -> std::result::Result<(), ConnError> {
        let chunk = Uint8Array::from(bytes);
        JsFuture::from(self.writer.write_with_chunk(&chunk))
            .await
            .map(drop)
            .map_err(conn_error)
    }

    fn finish(&mut self) -> std::result::Result<(), ConnError> {
        forget(self.writer.close());
        Ok(())
    }
}

impl MasterRecvStream for WebRecv {
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
