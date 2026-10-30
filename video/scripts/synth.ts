// Instruments and effects for the soundtrack, synthesized from scratch so
// the video needs no licensed audio. Mono instruments return a buffer;
// the score (sound.ts) places them in a stereo mix.

export const RATE = 48_000;

export type Stereo = { l: Float32Array; r: Float32Array };

export const stereo = (seconds: number): Stereo => ({
  l: new Float32Array(Math.ceil(seconds * RATE)),
  r: new Float32Array(Math.ceil(seconds * RATE)),
});

/** Mix `mono` into `out` at `t` seconds, with gain and pan (-1 left … 1 right). */
export const place = (out: Stereo, mono: Float32Array, t: number, gain: number, pan = 0) => {
  const start = Math.round(t * RATE);
  const left = gain * Math.cos(((pan + 1) * Math.PI) / 4);
  const right = gain * Math.sin(((pan + 1) * Math.PI) / 4);
  for (let i = 0; i < mono.length; i++) {
    const j = start + i;
    if (j < 0 || j >= out.l.length) continue;
    out.l[j] += mono[i] * left;
    out.r[j] += mono[i] * right;
  }
};

export const mixInto = (out: Stereo, src: Stereo, gain = 1) => {
  for (let i = 0; i < out.l.length; i++) {
    out.l[i] += src.l[i] * gain;
    out.r[i] += src.r[i] * gain;
  }
};

const hz = (midi: number) => 440 * Math.pow(2, (midi - 69) / 12);

// RBJ biquad, recomputed per block when its frequency moves.
type Biquad = { b0: number; b1: number; b2: number; a1: number; a2: number };

const biquad = (type: "lowpass" | "highpass" | "bandpass", freq: number, q: number): Biquad => {
  const w = (2 * Math.PI * Math.min(freq, RATE * 0.45)) / RATE;
  const cos = Math.cos(w);
  const alpha = Math.sin(w) / (2 * q);
  const a0 = 1 + alpha;
  const [b0, b1, b2] =
    type === "lowpass"
      ? [(1 - cos) / 2, 1 - cos, (1 - cos) / 2]
      : type === "highpass"
        ? [(1 + cos) / 2, -(1 + cos), (1 + cos) / 2]
        : [alpha, 0, -alpha];
  return { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: (-2 * cos) / a0, a2: (1 - alpha) / a0 };
};

/** Filter in place; `freq` may vary over time (seconds from the buffer's start). */
export const filter = (
  x: Float32Array,
  type: "lowpass" | "highpass" | "bandpass",
  freq: number | ((t: number) => number),
  q = 0.707,
) => {
  let c = biquad(type, typeof freq === "number" ? freq : freq(0), q);
  let x1 = 0;
  let x2 = 0;
  let y1 = 0;
  let y2 = 0;
  for (let i = 0; i < x.length; i++) {
    if (typeof freq === "function" && i % 32 === 0) c = biquad(type, freq(i / RATE), q);
    const y = c.b0 * x[i] + c.b1 * x1 + c.b2 * x2 - c.a1 * y1 - c.a2 * y2;
    x2 = x1;
    x1 = x[i];
    y2 = y1;
    y1 = y;
    x[i] = y;
  }
  return x;
};

const noise = (seconds: number, random: () => number) => {
  const x = new Float32Array(Math.ceil(seconds * RATE));
  for (let i = 0; i < x.length; i++) x[i] = random() * 2 - 1;
  return x;
};

const envelope = (x: Float32Array, attack: number, decay: number, delay = 0) => {
  for (let i = 0; i < x.length; i++) {
    const t = i / RATE - delay;
    x[i] *= t < 0 ? 0 : Math.min(1, t / attack) * Math.exp(-t / decay);
  }
  return x;
};

const add = (into: Float32Array, from: Float32Array, gain = 1) => {
  for (let i = 0; i < Math.min(into.length, from.length); i++) into[i] += from[i] * gain;
  return into;
};

/** A sine whose frequency glides from `from` to `to` with time constant `glide`. */
const sweep = (seconds: number, from: number, to: number, glide: number) => {
  const x = new Float32Array(Math.ceil(seconds * RATE));
  let phase = 0;
  for (let i = 0; i < x.length; i++) {
    const f = to + (from - to) * Math.exp(-i / RATE / glide);
    phase += (2 * Math.PI * f) / RATE;
    x[i] = Math.sin(phase);
  }
  return x;
};

// Keyboard: a bright click over a short low "thock", then the key's release.
export const key = (random: () => number, kind: "key" | "space" | "enter") => {
  const low = kind !== "key";
  const length = 0.12;
  const click = envelope(
    filter(noise(length, random), "bandpass", (low ? 2300 : 3400) + random() * 1600, 1.1),
    0.0004,
    0.0045,
  );
  const body = envelope(
    sweep(length, (low ? 150 : 230) * 1.7, (low ? 150 : 230) * (0.94 + random() * 0.12), 0.006),
    0.0008,
    low ? 0.03 : 0.017,
  );
  const release = envelope(
    filter(noise(length, random), "bandpass", 2600 + random() * 1200, 1.4),
    0.0003,
    0.003,
    0.05 + random() * 0.025,
  );
  const out = add(add(click, body, low ? 0.75 : 0.55), release, 0.32);
  return add(new Float32Array(out.length), out, kind === "enter" ? 1.15 : low ? 1 : 0.85);
};

// A paste: a soft, quick swish with a tap at the end.
export const paste = (random: () => number) => {
  const swish = filter(noise(0.16, random), "bandpass", (t) => 1800 + 9000 * t, 1.2);
  for (let i = 0; i < swish.length; i++) {
    const t = i / RATE;
    swish[i] *= Math.sin(Math.min(1, t / 0.16) * Math.PI) ** 2;
  }
  return add(swish, key(random, "key"), 0.6);
};

/** Air moving past: band-passed noise sweeping up or down, separate per ear. */
export const whoosh = (random: () => number, seconds = 0.75, rising = true): Stereo => {
  const out = stereo(seconds);
  for (const side of [out.l, out.r]) {
    const n = filter(
      noise(seconds, random),
      "bandpass",
      (t) => {
        const p = t / seconds;
        return 280 * Math.pow(9, rising ? p : 1 - p);
      },
      0.85,
    );
    for (let i = 0; i < n.length; i++) {
      const p = i / n.length;
      // Swells to a peak at 60%, then falls away.
      const shape = p < 0.6 ? Math.sin((p / 0.6) * (Math.PI / 2)) : Math.cos(((p - 0.6) / 0.4) * (Math.PI / 2));
      side[i] = n[i] * shape * shape;
    }
  }
  return out;
};

export const tick = (random: () => number) =>
  add(
    envelope(sweep(0.1, 2100, 1900 + random() * 120, 0.01), 0.001, 0.028),
    envelope(sweep(0.1, 4200, 3900, 0.01), 0.001, 0.012),
    0.3,
  );

export const pop = () => add(envelope(sweep(0.25, 880, 520, 0.018), 0.002, 0.07), envelope(sweep(0.25, 1760, 1040, 0.018), 0.002, 0.03), 0.25);

/** Success: two bell tones a fourth apart. */
export const chime = () => {
  const bell = (f: number, delay: number) => {
    const x = new Float32Array(Math.ceil(1.6 * RATE));
    for (let i = 0; i < x.length; i++) {
      const t = i / RATE - delay;
      if (t < 0) continue;
      const env = Math.min(1, t / 0.003) * Math.exp(-t / 0.42);
      x[i] =
        env *
        (Math.sin(2 * Math.PI * f * t) +
          0.28 * Math.sin(2 * Math.PI * f * 2.0 * t) * Math.exp(-t / 0.12) +
          0.12 * Math.sin(2 * Math.PI * f * 3.01 * t) * Math.exp(-t / 0.06));
    }
    return x;
  };
  return add(bell(hz(88), 0), bell(hz(93), 0.075), 0.8);
};

/** The logo: a deep, soft hit with an open chord blooming over it. */
export const logo = (random: () => number) => {
  const length = 3.2;
  const sub = envelope(sweep(length, 98, 49, 0.05), 0.004, 0.7);
  const bloom = new Float32Array(Math.ceil(length * RATE));
  [55, 62, 67, 74, 79].forEach((note, n) => {
    const f = hz(note);
    for (let i = 0; i < bloom.length; i++) {
      const t = i / RATE;
      bloom[i] += (Math.sin(2 * Math.PI * f * t) * Math.min(1, t / 0.02) * Math.exp(-t / (1.3 - n * 0.15))) / (n + 1.5);
    }
  });
  const air = envelope(filter(noise(length, random), "lowpass", 5000), 0.002, 0.18);
  return add(add(add(new Float32Array(sub.length), sub, 0.6), bloom, 0.55), air, 0.08);
};

/** A riser into a downbeat: noise and a gliding tone, opening up. */
export const swell = (random: () => number, seconds = 0.7) => {
  const x = filter(noise(seconds + 0.3, random), "lowpass", (t) => 400 + 7000 * Math.min(1, t / seconds) ** 2, 0.9);
  const tone = sweep(seconds + 0.3, hz(55), hz(67), 0.4);
  for (let i = 0; i < x.length; i++) {
    const t = i / RATE;
    const env = t < seconds ? (t / seconds) ** 2.2 : Math.exp(-(t - seconds) / 0.06);
    x[i] = (x[i] * 0.8 + tone[i] * 0.12) * env;
  }
  return x;
};

// --- Music ---------------------------------------------------------------

const polyblep = (phase: number, dt: number) => {
  if (phase < dt) {
    const t = phase / dt;
    return t + t - t * t - 1;
  }
  if (phase > 1 - dt) {
    const t = (phase - 1) / dt;
    return t * t + t + t + 1;
  }
  return 0;
};

/** A warm pad voice: two detuned band-limited saws, slow in and out. */
export const padVoice = (midi: number, seconds: number, attack: number, release: number) => {
  const length = seconds + release;
  const x = new Float32Array(Math.ceil(length * RATE));
  const voices = [-0.07, 0.07].map((cents) => hz(midi) * Math.pow(2, cents / 12));
  const phases = [0.13, 0.61];
  for (let i = 0; i < x.length; i++) {
    const t = i / RATE;
    let v = 0;
    voices.forEach((f, n) => {
      const dt = f / RATE;
      phases[n] = (phases[n] + dt) % 1;
      v += 2 * phases[n] - 1 - polyblep(phases[n], dt);
    });
    const env = Math.min(1, t / attack) * (t > seconds ? Math.exp(-(t - seconds) / (release / 3)) : 1);
    x[i] = v * 0.5 * env;
  }
  return x;
};

/** A soft mallet for the arpeggio. */
export const pluck = (midi: number) => {
  const f = hz(midi);
  const x = new Float32Array(Math.ceil(0.9 * RATE));
  for (let i = 0; i < x.length; i++) {
    const t = i / RATE;
    const env = Math.min(1, t / 0.004) * Math.exp(-t / 0.2);
    x[i] = env * (Math.sin(2 * Math.PI * f * t) + 0.18 * Math.sin(2 * Math.PI * 2 * f * t) * Math.exp(-t / 0.05));
  }
  return x;
};

export const bass = (midi: number, seconds: number) => {
  const f = hz(midi);
  const x = new Float32Array(Math.ceil((seconds + 0.3) * RATE));
  for (let i = 0; i < x.length; i++) {
    const t = i / RATE;
    const env = Math.min(1, t / 0.015) * (t > seconds ? Math.exp(-(t - seconds) / 0.08) : Math.exp(-t / 2.5));
    x[i] = env * Math.tanh(1.6 * Math.sin(2 * Math.PI * f * t)) * 0.6;
  }
  return x;
};

export const kick = () => envelope(sweep(0.5, 120, 46, 0.035), 0.002, 0.16);

export const hat = (random: () => number) =>
  envelope(filter(noise(0.08, random), "highpass", 7500, 0.8), 0.0005, 0.018);

/**
 * A small stereo reverb: eight combs into two all-passes per side
 * (Freeverb's structure), for the room the sounds sit in.
 */
export const reverb = (dry: Stereo, size = 0.82, damp = 0.3): Stereo => {
  const out = stereo(dry.l.length / RATE);
  const combs = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
  const passes = [556, 441];
  ([
    [dry.l, out.l, 0],
    [dry.r, out.r, 23],
  ] as const).forEach(([input, output, spread]) => {
    for (const d of combs) {
      const delay = Math.round(((d + spread) * RATE) / 44100);
      const buf = new Float32Array(delay);
      let index = 0;
      let store = 0;
      for (let i = 0; i < input.length; i++) {
        const y = buf[index];
        store = y * (1 - damp) + store * damp;
        buf[index] = input[i] * 0.015 + store * size;
        index = (index + 1) % delay;
        output[i] += y;
      }
    }
    for (const d of passes) {
      const delay = Math.round(((d + spread) * RATE) / 44100);
      const buf = new Float32Array(delay);
      let index = 0;
      for (let i = 0; i < output.length; i++) {
        const b = buf[index];
        const y = -output[i] + b;
        buf[index] = output[i] + b * 0.5;
        index = (index + 1) % delay;
        output[i] = y;
      }
    }
  });
  return out;
};

/** 16-bit stereo PCM WAV. */
export const wav = (mix: Stereo): Uint8Array => {
  const frames = mix.l.length;
  const bytes = new Uint8Array(44 + frames * 4);
  const view = new DataView(bytes.buffer);
  const text = (offset: number, s: string) => [...s].forEach((c, i) => view.setUint8(offset + i, c.charCodeAt(0)));
  text(0, "RIFF");
  view.setUint32(4, 36 + frames * 4, true);
  text(8, "WAVE");
  text(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 2, true);
  view.setUint32(24, RATE, true);
  view.setUint32(28, RATE * 4, true);
  view.setUint16(32, 4, true);
  view.setUint16(34, 16, true);
  text(36, "data");
  view.setUint32(40, frames * 4, true);
  for (let i = 0; i < frames; i++) {
    view.setInt16(44 + i * 4, Math.round(Math.max(-1, Math.min(1, mix.l[i])) * 32767), true);
    view.setInt16(46 + i * 4, Math.round(Math.max(-1, Math.min(1, mix.r[i])) * 32767), true);
  }
  return bytes;
};

export const placeStereo = (out: Stereo, src: Stereo, t: number, gain: number) => {
  const start = Math.round(t * RATE);
  for (let i = 0; i < src.l.length; i++) {
    const j = start + i;
    if (j < 0 || j >= out.l.length) continue;
    out.l[j] += src.l[i] * gain;
    out.r[j] += src.r[i] * gain;
  }
};
