// The soundtrack: music and effects scored to the same timeline as the
// frames, so every keystroke lands on its frame. Writes
// public/audio/<launch id>.wav for each launch. Run by `npm run sound`.

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { launches } from "../src/demos";
import { rng } from "../src/lib/random";
import { type Timeline, buildTimeline } from "../src/lib/timeline";
import { loadCaptures } from "./captures";
import * as synth from "./synth";

const BEAT = 0.6; // 100 bpm
const BAR = 4 * BEAT;
const DOWNBEAT = 0.3; // the logo's hit, with the headline
// Two bars each, round and round: Fmaj7, G6, Am7, Cmaj9.
const CHORDS = [
  [53, 57, 60, 64],
  [55, 59, 62, 64],
  [57, 60, 64, 67],
  [48, 55, 59, 62, 64],
];
const ROOTS = [41, 43, 45, 36];
const HOME = 3; // Cmaj9 closes the video

const score = (timeline: Timeline) => {
  const { marks } = timeline;
  const length = timeline.duration + 0.4;
  const random = rng(`${timeline.launch.id}:sound`);
  const pad = synth.stereo(length);
  const music = synth.stereo(length);
  const fx = synth.stereo(length);
  const room = synth.stereo(length);

  // Pads: a chord every two bars, then home at the end card.
  const end = marks["end.start"];
  for (let k = 0; DOWNBEAT + k * 2 * BAR < end; k++) {
    const start = DOWNBEAT + k * 2 * BAR;
    const chord = CHORDS[k % CHORDS.length];
    const hold = Math.min(2 * BAR, end - start);
    chord.forEach((note, n) => {
      synth.place(pad, synth.padVoice(note, hold, k === 0 ? 0.9 : 0.5, 1.4), start, 0.05, n % 2 ? 0.35 : -0.35);
    });
  }
  CHORDS[HOME].forEach((note, n) => {
    synth.place(pad, synth.padVoice(note, length - end, 0.25, 1.0), end, 0.055, n % 2 ? 0.4 : -0.4);
  });
  // The pad is dark under the title, opens with the demo, and blooms for the tagline.
  const cutoff = (t: number) =>
    t < marks["terminal.start"] ? 650 : t < marks["tagline.start"] ? 1050 : t < end ? 1050 + 2400 * Math.min(1, (t - marks["tagline.start"]) / 1.6) : 2600;
  synth.filter(pad.l, "lowpass", cutoff, 0.8);
  synth.filter(pad.r, "lowpass", cutoff, 0.8);
  synth.mixInto(music, pad);
  synth.mixInto(room, pad, 0.5);

  // The groove: bass, kick, an arpeggio and quiet hats, from the first
  // command until the tagline.
  const grid = (t: number) => DOWNBEAT + Math.ceil((t - DOWNBEAT) / BEAT - 1e-6) * BEAT;
  const from = grid(marks["balance.start"]);
  const until = marks["tagline.start"];
  const chordAt = (t: number) => Math.floor((t - DOWNBEAT) / (2 * BAR)) % CHORDS.length;
  for (let t = from; t < until - 0.01; t += BEAT) {
    const beat = Math.round((t - DOWNBEAT) / BEAT) % 4;
    const fadeIn = Math.min(1, (t - from) / (2 * BAR) + 0.35);
    if (beat === 0 || beat === 2) synth.place(music, synth.kick(), t, 0.2 * fadeIn);
    if (beat === 0) synth.place(music, synth.bass(ROOTS[chordAt(t)], BAR - 0.1), t, 0.08 * fadeIn);
    for (const half of [0, 0.5]) {
      const at = t + half * BEAT;
      if (at >= until) continue;
      const chord = CHORDS[chordAt(at)];
      const step = Math.round((at - DOWNBEAT) / (BEAT / 2));
      const order = [0, 2, 1, 3, 2, 1, 3, 1];
      const note = chord[order[step % order.length] % chord.length] + 12;
      const pluck = synth.pluck(note);
      synth.place(music, pluck, at, 0.06 * fadeIn, step % 2 ? 0.35 : -0.35);
      synth.place(room, pluck, at, 0.05 * fadeIn, step % 2 ? -0.3 : 0.3);
      if (half === 0.5 && at >= marks["send.start"]) synth.place(music, synth.hat(random), at, 0.03, 0.2);
    }
  }
  // Into the end card.
  synth.place(music, synth.swell(random, 0.8), end - 0.8, 0.22);

  // Effects, from the same events the frames animate.
  for (const sound of timeline.sounds) {
    const r = rng(`${sound.seed}`);
    const pan = (r() - 0.5) * 0.3;
    switch (sound.kind) {
      case "key":
      case "space":
      case "enter":
        synth.place(fx, synth.key(r, sound.kind), sound.t, sound.kind === "key" ? 0.2 : 0.26, pan);
        break;
      case "paste":
        synth.place(fx, synth.paste(r), sound.t - 0.08, 0.28, pan);
        break;
      case "whoosh":
        synth.placeStereo(fx, synth.whoosh(r), sound.t - 0.35, 0.3);
        break;
      case "tick":
        synth.place(fx, synth.tick(r), sound.t, 0.07, pan);
        synth.place(room, synth.tick(r), sound.t, 0.06);
        break;
      case "pop":
        synth.place(fx, synth.pop(), sound.t, 0.16, pan);
        synth.place(room, synth.pop(), sound.t, 0.12);
        break;
      case "chime":
        synth.place(fx, synth.chime(), sound.t, 0.12);
        synth.place(room, synth.chime(), sound.t, 0.16);
        break;
      case "logo":
        synth.place(fx, synth.logo(r), sound.t, 0.42);
        synth.place(room, synth.logo(r), sound.t, 0.3);
        break;
    }
  }

  const mix = synth.stereo(length);
  synth.mixInto(mix, music);
  synth.mixInto(mix, fx);
  synth.mixInto(mix, synth.reverb(room), 0.9);
  // Master: no rumble, gentle saturation, a short fade, then -1 dBFS peak.
  synth.filter(mix.l, "highpass", 36);
  synth.filter(mix.r, "highpass", 36);
  let peak = 0;
  for (const side of [mix.l, mix.r]) {
    for (let i = 0; i < side.length; i++) {
      const t = i / synth.RATE;
      const fade = Math.min(1, Math.max(0, (timeline.duration - t) / 0.7));
      side[i] = Math.tanh(side[i] * 1.2) * fade;
      peak = Math.max(peak, Math.abs(side[i]));
    }
  }
  const gain = 0.89 / peak;
  for (const side of [mix.l, mix.r]) for (let i = 0; i < side.length; i++) side[i] *= gain;
  return mix;
};

const out = join(import.meta.dirname, "..", "public", "audio");
mkdirSync(out, { recursive: true });
const captures = loadCaptures();
for (const launch of launches) {
  const timeline = buildTimeline(launch, captures);
  const file = join(out, `${launch.id}.wav`);
  writeFileSync(file, synth.wav(score(timeline)));
  console.log(`${file}  ${timeline.duration.toFixed(2)}s`);
}
