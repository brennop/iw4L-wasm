// AudioWorklet mixer (U8, experimental). Holds clip PCM and a voice table and mixes them on the
// audio render thread, so a stalled main thread can delay a new sound but never interrupts one
// that already plays. The game drives it through `node.port` only (no SharedArrayBuffer).
//
// Messages to the worklet, one per game frame at most:
//   { clips: [{ id, ch, rate, pcm: ArrayBuffer }],   // interleaved f32, registered once per clip
//     free: [id],                                     // clips the game no longer holds
//     cmds: Float32Array }                            // records of CMD_STRIDE floats, below
// A record is [op, voice, clip, flags, gainL, gainR, rate]. Ops: 1 start, 2 set, 3 stop.
// flags: bit 0 loop, bit 1 paused. rate is the playback speed (pitch), 1 = the clip's own rate.
// Messages from the worklet:
//   { finished: [voice] }                             // one-shots that ran out (or could not start)
//   { stats: { voices, clips } }              // about once a second

const CMD_START = 1;
const CMD_SET = 2;
const CMD_STOP = 3;
const CMD_STRIDE = 7;
const MAX_VOICES = 160;
const STATS_BLOCKS = 375;

class Iw4lMixer extends AudioWorkletProcessor {
  constructor() {
    super();
    this.clips = new Map();
    this.voices = new Map();
    this.finished = [];
    this.blocks = 0;
    this.port.onmessage = (event) => this.onMessage(event.data);
  }

  onMessage(msg) {
    if (msg.clips) {
      for (const clip of msg.clips) {
        const data = new Float32Array(clip.pcm);
        this.clips.set(clip.id, { ch: clip.ch, rate: clip.rate, data, frames: Math.floor(data.length / clip.ch) });
      }
    }
    if (msg.free) {
      for (const id of msg.free) this.clips.delete(id);
    }
    const cmds = msg.cmds;
    if (!cmds) return;
    for (let i = 0; i + CMD_STRIDE <= cmds.length; i += CMD_STRIDE) {
      const op = cmds[i];
      const id = cmds[i + 1];
      if (op === CMD_START) {
        this.start(id, cmds[i + 2], cmds[i + 3], cmds[i + 4], cmds[i + 5], cmds[i + 6]);
      } else if (op === CMD_SET) {
        const voice = this.voices.get(id);
        if (!voice) continue;
        const flags = cmds[i + 3];
        voice.paused = (flags & 2) !== 0;
        voice.tl = cmds[i + 4];
        voice.tr = cmds[i + 5];
        voice.step = (voice.clip.rate / sampleRate) * cmds[i + 6];
      } else if (op === CMD_STOP) {
        this.voices.delete(id);
      }
    }
  }

  start(id, clipId, flags, gl, gr, speed) {
    const clip = this.clips.get(clipId);
    if (!clip || clip.frames === 0 || this.voices.size >= MAX_VOICES) {
      this.finished.push(id);
      return;
    }
    this.voices.set(id, {
      clip,
      pos: 0,
      step: (clip.rate / sampleRate) * speed,
      loop: (flags & 1) !== 0,
      paused: (flags & 2) !== 0,
      // First block ramps from silence so a start never clicks.
      gl: 0,
      gr: 0,
      tl: gl,
      tr: gr,
    });
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    const left = out[0];
    const right = out[1] || out[0];
    const n = left.length;
    left.fill(0);
    if (right !== left) right.fill(0);

    for (const [id, v] of this.voices) {
      if (v.paused) continue;
      const { data, ch, frames } = v.clip;
      const step = v.step;
      const dl = (v.tl - v.gl) / n;
      const dr = (v.tr - v.gr) / n;
      let pos = v.pos;
      let gl = v.gl;
      let gr = v.gr;
      let ended = false;
      for (let i = 0; i < n; i++) {
        let i0 = pos | 0;
        if (i0 >= frames) {
          if (!v.loop) {
            ended = true;
            break;
          }
          pos -= frames;
          i0 = pos | 0;
        }
        const frac = pos - i0;
        let i1 = i0 + 1;
        if (i1 >= frames) i1 = v.loop ? 0 : i0;
        let l;
        let r;
        if (ch === 1) {
          l = r = data[i0] + (data[i1] - data[i0]) * frac;
        } else {
          const a = i0 * ch;
          const b = i1 * ch;
          l = data[a] + (data[b] - data[a]) * frac;
          r = data[a + 1] + (data[b + 1] - data[a + 1]) * frac;
        }
        gl += dl;
        gr += dr;
        left[i] += l * gl;
        right[i] += r * gr;
        pos += step;
      }
      v.pos = pos;
      v.gl = v.tl;
      v.gr = v.tr;
      if (ended) {
        this.voices.delete(id);
        this.finished.push(id);
      }
    }

    if (this.finished.length) {
      this.port.postMessage({ finished: this.finished });
      this.finished = [];
    }
    this.blocks++;
    if (this.blocks >= STATS_BLOCKS) {
      this.blocks = 0;
      this.port.postMessage({ stats: { voices: this.voices.size, clips: this.clips.size } });
    }
    return true;
  }
}

registerProcessor('iw4l-mixer', Iw4lMixer);
