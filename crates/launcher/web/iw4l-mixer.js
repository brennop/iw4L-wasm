// AudioWorklet ring-buffer player (`iw4l-mixer`). The game mixes on the main thread (audio
// crate, web_output.rs) and posts finished 48 kHz stereo blocks here; this only plays them.
// The game keeps a small margin buffered ahead, so a stalled main thread runs this dry and it
// plays silence (an underrun) rather than the game rendering a burst to catch up.
//
// Messages to the worklet:
//   { pcm: Float32Array }   // interleaved L/R frames, a whole number of 128-frame blocks
// Messages from the worklet, every REPORT_QUANTA render quanta while playing:
//   { consumed,             // frames played out of the ring since the node started (cumulative)
//     time,                 // `currentTime` when the report was made
//     buffered,             // frames still in the ring
//     underruns,            // render quanta that found the ring short (cumulative, after the first block)
//     underrunFrames,       // frames of silence those quanta played (cumulative)
//     overflowFrames,       // frames dropped because the ring was full (cumulative)
//     peak, sumsq, n }      // output level over the frames since the last report

const RING_FRAMES = 48000; // 1 s at 48 kHz, far above any margin the game keeps
const REPORT_QUANTA = 8; // ~21 ms

class Iw4lMixer extends AudioWorkletProcessor {
  constructor() {
    super();
    this.ring = new Float32Array(RING_FRAMES * 2);
    this.read = 0; // frame index into the ring
    this.fill = 0; // frames buffered
    this.consumed = 0;
    this.underruns = 0;
    this.underrunFrames = 0;
    this.overflowFrames = 0;
    this.started = false;
    this.quanta = 0;
    this.peak = 0;
    this.sumsq = 0;
    this.n = 0;
    this.port.onmessage = (event) => this.push(event.data.pcm);
  }

  push(pcm) {
    if (!pcm) return;
    const frames = pcm.length >> 1;
    const room = RING_FRAMES - this.fill;
    const take = Math.min(frames, room);
    this.overflowFrames += frames - take;
    let write = (this.read + this.fill) % RING_FRAMES;
    let offset = 0;
    while (offset < take) {
      const run = Math.min(take - offset, RING_FRAMES - write);
      this.ring.set(pcm.subarray(offset * 2, (offset + run) * 2), write * 2);
      offset += run;
      write = (write + run) % RING_FRAMES;
    }
    this.fill += take;
    if (take > 0) this.started = true;
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    const left = out[0];
    const right = out[1] || out[0];
    const n = left.length;
    const take = Math.min(n, this.fill);
    const ring = this.ring;
    let read = this.read;
    let peak = this.peak;
    let sumsq = this.sumsq;
    for (let i = 0; i < take; i++) {
      const l = ring[read * 2];
      const r = ring[read * 2 + 1];
      left[i] = l;
      right[i] = r;
      const al = l < 0 ? -l : l;
      const ar = r < 0 ? -r : r;
      if (al > peak) peak = al;
      if (ar > peak) peak = ar;
      sumsq += l * l + r * r;
      read++;
      if (read === RING_FRAMES) read = 0;
    }
    for (let i = take; i < n; i++) {
      left[i] = 0;
      right[i] = 0;
    }
    this.read = read;
    this.fill -= take;
    this.consumed += take;
    this.peak = peak;
    this.sumsq = sumsq;
    this.n += take;
    if (take < n && this.started) {
      this.underruns++;
      this.underrunFrames += n - take;
    }
    if (++this.quanta >= REPORT_QUANTA) {
      this.quanta = 0;
      this.port.postMessage({
        consumed: this.consumed,
        time: currentTime,
        buffered: this.fill,
        underruns: this.underruns,
        underrunFrames: this.underrunFrames,
        overflowFrames: this.overflowFrames,
        peak: this.peak,
        sumsq: this.sumsq,
        n: this.n,
      });
      this.peak = 0;
      this.sumsq = 0;
      this.n = 0;
    }
    return true;
  }
}

registerProcessor('iw4l-mixer', Iw4lMixer);
