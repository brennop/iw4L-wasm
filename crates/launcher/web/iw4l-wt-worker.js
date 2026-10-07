// O19: the browser's WebTransport connection to the master, owned by a dedicated worker.
//
// Chrome's datagram writable needs main-thread turns to hand each datagram to the network
// process (O15, O18), so a busy game frame turns several datagrams per frame into about one
// per frame and then a burst. A WebTransport session made in a worker has no such backlog.
//
// The page (crates/net/src/transport/master/conn_ws.rs, `Pipe::Worker`) and this worker speak
// the frames of the WebSocket transport over postMessage, so the page reuses its ws backend:
//   page -> worker  ArrayBuffer (transferred): 1 prefix byte, then a ws_frame.
//                   prefix bit 0 = droppable upstream datagram (hop_probe::upstream_droppable),
//                   bit 1 = probe: 8 bytes of LE hash follow the prefix, before the frame
//                   (hop_probe::worker_envelope).
//   worker -> page  ArrayBuffer (transferred): a bare ws_frame.
//   control         plain objects with a `type` field:
//     page -> worker  {type:"connect", url, certHash, options:{congestionControl, upDrop, bpThresh}}
//     worker -> page  {type:"ready", maxDatagramSize, congestionControl}
//                     {type:"error", message}
//                     {type:"probe", hash, len, unix_ms, write_ms, desired_size}
//                     {type:"stats", written, dropped, min_desired, oversize}   (every 5 s, at close)
//
// ws_frame layout (crates/master_protocol/src/ws_frame.rs): u8 kind, then a little-endian u32
// stream id for the stream kinds, then the payload; Close is a u32 LE code and the reason bytes.
'use strict';

// L2: wall-clock stamps (epoch ms) so the page can place the worker on its own clock.
const scriptAt = performance.timeOrigin + performance.now();
let connectAt = 0;

const KIND_DATAGRAM = 0;
const KIND_OPEN_BI = 1;
const KIND_OPEN_UNI = 2;
const KIND_DATA = 3;
const KIND_FIN = 4;
const KIND_STOP = 5;
const KIND_CLOSE = 6;
const MAX_DATA_CHUNK = 16 * 1024; // ws_frame::MAX_DATA_CHUNK
const STATS_INTERVAL_MS = 5000;

let transport = null;
let datagramWriter = null;
let ready = false;
let closed = false; // transport.closed settled or the page asked to close
let failed = false; // an error was already reported
let maxDatagramSize = 1024;
let dropBp = false;
let bpThresh = -1;
let written = 0;
let dropped = 0;
let oversize = 0;
let minDesired = null;
let nextUni = 1; // the master's ws side numbers its uni streams from 1, odd
let statsTimer = 0;
const streams = new Map(); // bidi id -> { writer, pending: [() => Promise] }
const encoder = new TextEncoder();
const decoder = new TextDecoder();

function describe(error) {
  if (error && error.name) return `${error.name}: ${error.message}`;
  return String(error);
}

// One report per worker: every later failure follows from the first.
function fail(message) {
  if (failed) return;
  failed = true;
  self.postMessage({ type: 'error', message });
}

// A rejection while the session is closing or closed is part of the close, not an error.
function failUnlessClosed(what, error) {
  if (!closed) fail(`${what}: ${describe(error)}`);
}

function post(buffer) {
  self.postMessage(buffer, [buffer]);
}

// A ws_frame: kind, optional u32 LE stream id, payload.
function frame(kind, id, payload) {
  const head = id === null ? 1 : 5;
  const buffer = new ArrayBuffer(head + (payload ? payload.length : 0));
  const view = new DataView(buffer);
  view.setUint8(0, kind);
  if (id !== null) view.setUint32(1, id, true);
  if (payload && payload.length) new Uint8Array(buffer, head).set(payload);
  return buffer;
}

function closeFrame(code, reason) {
  const bytes = encoder.encode(reason);
  const buffer = new ArrayBuffer(5 + bytes.length);
  const view = new DataView(buffer);
  view.setUint8(0, KIND_CLOSE);
  view.setUint32(1, code >>> 0, true);
  new Uint8Array(buffer, 5).set(bytes);
  return buffer;
}

// Splits a stream chunk into DATA frames of at most MAX_DATA_CHUNK.
function postData(id, chunk) {
  for (let at = 0; at < chunk.length; at += MAX_DATA_CHUNK) {
    post(frame(KIND_DATA, id, chunk.subarray(at, at + MAX_DATA_CHUNK)));
  }
}

async function pump(reader, onChunk) {
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return;
    onChunk(value);
  }
}

function postStats() {
  self.postMessage({ type: 'stats', written, dropped, min_desired: minDesired, oversize });
}

async function connect(message) {
  connectAt = performance.timeOrigin + performance.now();
  const options = {};
  if (message.certHash) {
    options.serverCertificateHashes = [{ algorithm: 'sha-256', value: message.certHash }];
  }
  const extra = message.options || {};
  if (extra.congestionControl) options.congestionControl = extra.congestionControl;
  dropBp = extra.upDrop === 'bp';
  if (typeof extra.bpThresh === 'number') bpThresh = extra.bpThresh;

  transport = new WebTransport(message.url, options);
  transport.closed.then(
    (info) => {
      closed = true;
      post(closeFrame(info.closeCode || 0, info.reason || ''));
    },
    (error) => {
      closed = true;
      post(closeFrame(0, describe(error)));
    },
  );
  await transport.ready;

  const datagrams = transport.datagrams;
  datagramWriter = datagrams.writable.getWriter();
  maxDatagramSize = datagrams.maxDatagramSize;
  pump(datagrams.readable.getReader(), (value) => post(frame(KIND_DATAGRAM, null, value)))
    .catch((error) => failUnlessClosed('datagram read', error));
  pump(transport.incomingUnidirectionalStreams.getReader(), (stream) => {
    const id = nextUni;
    nextUni += 2;
    post(frame(KIND_OPEN_UNI, id, null));
    pump(stream.getReader(), (chunk) => postData(id, chunk))
      .then(() => post(frame(KIND_FIN, id, null)))
      .catch((error) => failUnlessClosed(`uni stream ${id} read`, error));
  }).catch((error) => failUnlessClosed('incoming uni streams', error));

  ready = true;
  statsTimer = setInterval(postStats, STATS_INTERVAL_MS);
  self.postMessage({
    type: 'ready',
    scriptAt,
    connectAt,
    readyAt: performance.timeOrigin + performance.now(),
    maxDatagramSize,
    congestionControl: transport.congestionControl,
  });
}

function openBi(id) {
  const entry = { writer: null, pending: [] };
  streams.set(id, entry);
  transport.createBidirectionalStream().then((stream) => {
    entry.writer = stream.writable.getWriter();
    for (const op of entry.pending.splice(0)) op();
    pump(stream.readable.getReader(), (chunk) => postData(id, chunk))
      .then(() => post(frame(KIND_FIN, id, null)))
      .catch((error) => failUnlessClosed(`bidi stream ${id} read`, error));
  }).catch((error) => failUnlessClosed(`createBidirectionalStream ${id}`, error));
}

// Runs `op` on the stream's writer, once the stream exists. A stream writer queues its
// writes, so they stay in the order the page sent them.
function onStream(id, what, op) {
  const entry = streams.get(id);
  if (!entry) {
    fail(`${what} for unknown stream ${id}`);
    return;
  }
  const run = () => op(entry.writer).catch((error) => failUnlessClosed(`${what} stream ${id}`, error));
  if (entry.writer) run();
  else entry.pending.push(run);
}

function sendDatagram(buffer, at, flags, hash) {
  if (buffer.byteLength - at > maxDatagramSize) {
    oversize += 1;
    return;
  }
  const desired = datagramWriter.desiredSize;
  if (desired !== null && (minDesired === null || desired < minDesired)) minDesired = desired;
  if (dropBp && (flags & 1) !== 0 && desired !== null && desired <= bpThresh) {
    dropped += 1;
    return;
  }
  written += 1;
  const started = performance.now();
  const len = buffer.byteLength - at;
  const promise = datagramWriter.write(new Uint8Array(buffer, at));
  const report = () => {
    if (hash === null) return;
    self.postMessage({
      type: 'probe',
      hash,
      len,
      unix_ms: Date.now(),
      write_ms: Math.floor(performance.now() - started),
      desired_size: desired,
    });
  };
  promise.then(report, (error) => {
    report();
    failUnlessClosed('datagram write', error);
  });
}

function onFrame(buffer) {
  if (!ready) {
    fail('frame before ready');
    return;
  }
  const view = new DataView(buffer);
  const flags = view.getUint8(0);
  let at = 1;
  let hash = null;
  if (flags & 2) {
    const lo = view.getUint32(1, true);
    const hi = view.getUint32(5, true);
    hash = hi.toString(16).padStart(8, '0') + lo.toString(16).padStart(8, '0');
    at = 9;
  }
  const kind = view.getUint8(at);
  switch (kind) {
    case KIND_DATAGRAM:
      sendDatagram(buffer, at + 1, flags, hash);
      break;
    case KIND_OPEN_BI:
      openBi(view.getUint32(at + 1, true));
      break;
    case KIND_DATA: {
      const id = view.getUint32(at + 1, true);
      const payload = new Uint8Array(buffer, at + 5);
      onStream(id, 'write', (writer) => writer.write(payload));
      break;
    }
    case KIND_FIN:
      onStream(view.getUint32(at + 1, true), 'close', (writer) => writer.close());
      break;
    case KIND_CLOSE: {
      const code = view.getUint32(at + 1, true);
      const reason = decoder.decode(new Uint8Array(buffer, at + 5));
      closed = true;
      clearInterval(statsTimer);
      postStats();
      transport.close({ closeCode: code, reason });
      break;
    }
    case KIND_OPEN_UNI:
    case KIND_STOP:
    default:
      fail(`unsupported page frame kind ${kind}`);
  }
}

self.onmessage = (event) => {
  const data = event.data;
  try {
    if (data instanceof ArrayBuffer) {
      onFrame(data);
    } else if (data && data.type === 'connect') {
      connect(data).catch((error) => fail(`connect: ${describe(error)}`));
    } else {
      fail(`unknown message ${JSON.stringify(data)}`);
    }
  } catch (error) {
    fail(`message handler: ${describe(error)}`);
  }
};

// Nothing may be silent: an escaped rejection or error becomes an error message too.
self.onunhandledrejection = (event) => fail(`unhandled rejection: ${describe(event.reason)}`);
self.onerror = (message) => fail(`worker error: ${message}`);
