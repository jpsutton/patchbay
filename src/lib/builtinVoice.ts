/**
 * A small English voice that plays in the page, so Linux, a browser, and a phone with no
 * system voice still speak. It is a formant sketch, not a recorded voice: vowels are two or
 * three tones, consonants are noise, and unknown words follow a short spelling table.
 */

export const SAMPLE_RATE = 22050;

export type SpeechRate = "slower" | "normal" | "faster";

const SCALE: Record<SpeechRate, number> = { slower: 1.28, normal: 1, faster: 0.78 };

type Phone = { f1: number; f2: number; f3: number; ms: number; kind: "vowel" | "noise" | "burst" | "nasal" | "gap" };

const VOWEL: Record<string, [number, number, number]> = {
  IY: [270, 2290, 3010],
  IH: [390, 1990, 2550],
  EH: [530, 1840, 2480],
  AE: [660, 1720, 2410],
  AA: [730, 1090, 2440],
  AH: [640, 1190, 2390],
  AO: [570, 840, 2410],
  UH: [440, 1020, 2240],
  UW: [300, 870, 2240],
  ER: [490, 1350, 1690],
};

const WORDS: Record<string, string> = {
  a: "AH",
  an: "AE N",
  and: "AE N D",
  the: "DH AH",
  to: "T UW",
  of: "AH V",
  you: "Y UW",
  your: "Y AO R",
  is: "IH Z",
  it: "IH T",
  this: "DH IH S",
  that: "DH AE T",
  hello: "HH EH L OW",
  code: "K OW D",
  block: "B L AA K",
  diagram: "D AY AH G R AE M",
  patchbay: "P AE CH B EY",
};

const RULES: [string, string][] = [
  ["tion", "SH AH N"],
  ["ight", "AY T"],
  ["ough", "AO"],
  ["eigh", "EY"],
  ["augh", "AO"],
  ["tch", "CH"],
  ["dge", "JH"],
  ["ing", "IH NG"],
  ["ong", "AO NG"],
  ["ang", "AE NG"],
  ["ung", "AH NG"],
  ["the", "DH AH"],
  ["she", "SH IY"],
  ["ee", "IY"],
  ["ea", "IY"],
  ["oo", "UW"],
  ["ai", "EY"],
  ["ay", "EY"],
  ["oa", "OW"],
  ["ow", "OW"],
  ["ou", "AW"],
  ["oi", "OY"],
  ["oy", "OY"],
  ["au", "AO"],
  ["aw", "AO"],
  ["sh", "SH"],
  ["ch", "CH"],
  ["th", "TH"],
  ["ph", "F"],
  ["wh", "W"],
  ["qu", "K W"],
  ["ck", "K"],
  ["ng", "NG"],
];

const LETTER: Record<string, string> = {
  a: "AE",
  b: "B",
  c: "K",
  d: "D",
  e: "EH",
  f: "F",
  g: "G",
  h: "HH",
  i: "IH",
  j: "JH",
  k: "K",
  l: "L",
  m: "M",
  n: "N",
  o: "AA",
  p: "P",
  q: "K",
  r: "R",
  s: "S",
  t: "T",
  u: "AH",
  v: "V",
  w: "W",
  x: "K S",
  y: "Y",
  z: "Z",
};

/** PCM for `text` at the given speed. Silent when there is nothing to say. */
export function renderSpeech(text: string, rate: SpeechRate = "normal"): Float32Array {
  const phones = textToPhones(text);
  if (phones.length === 0) return new Float32Array();
  const scale = SCALE[rate];
  const total = phones.reduce((n, p) => n + Math.round(((p.ms * scale) / 1000) * SAMPLE_RATE), 0);
  const out = new Float32Array(total);
  let at = 0;
  for (const p of phones) {
    const len = Math.max(1, Math.round(((p.ms * scale) / 1000) * SAMPLE_RATE));
    writePhone(out, at, len, p);
    at += len;
  }
  return out;
}

function textToPhones(text: string): Phone[] {
  const phones: Phone[] = [];
  const words = text.toLowerCase().split(/[^a-z0-9']+/).filter(Boolean);
  words.forEach((word, i) => {
    if (i > 0) phones.push({ f1: 0, f2: 0, f3: 0, ms: 50, kind: "gap" });
    for (const token of spell(word)) phones.push(phoneOf(token));
  });
  return phones;
}

function spell(word: string): string[] {
  const known = WORDS[word];
  if (known) return known.split(" ");
  const out: string[] = [];
  let i = 0;
  while (i < word.length) {
    const rest = word.slice(i);
    const rule = RULES.find(([g]) => rest.startsWith(g));
    if (rule) {
      out.push(...rule[1].split(" "));
      i += rule[0].length;
      continue;
    }
    const letter = LETTER[word[i]];
    if (letter) out.push(...letter.split(" "));
    i += 1;
  }
  if (word.endsWith("e") && out.length > 1 && out[out.length - 1] === "EH") out.pop();
  return out;
}

function phoneOf(token: string): Phone {
  const diph = diphthong(token);
  if (diph) return diph;
  const vowel = VOWEL[token];
  if (vowel) return { f1: vowel[0], f2: vowel[1], f3: vowel[2], ms: 110, kind: "vowel" };
  if (token === "M" || token === "N" || token === "NG") return { f1: 280, f2: 1400, f3: 2500, ms: 70, kind: "nasal" };
  if (token === "S" || token === "Z" || token === "F" || token === "V" || token === "SH" || token === "ZH" || token === "TH" || token === "DH" || token === "HH") {
    return { f1: 0, f2: fricative(token), f3: 0, ms: 80, kind: "noise" };
  }
  if (token === "L" || token === "R" || token === "W" || token === "Y") {
    return { f1: 400, f2: token === "Y" ? 2200 : 1200, f3: 2400, ms: 60, kind: "vowel" };
  }
  return { f1: 0, f2: 0, f3: 0, ms: 45, kind: "burst" };
}

function diphthong(token: string): Phone | null {
  const map: Record<string, [string, string]> = { EY: ["EH", "IY"], AY: ["AA", "IY"], OW: ["AO", "UW"], AW: ["AA", "UH"], OY: ["AO", "IY"] };
  const pair = map[token];
  if (!pair) return null;
  const a = VOWEL[pair[0]];
  const b = VOWEL[pair[1]];
  return { f1: a[0], f2: a[1], f3: b[1], ms: 140, kind: "vowel" };
}

function fricative(token: string): number {
  switch (token) {
    case "S":
    case "Z":
      return 5500;
    case "SH":
    case "ZH":
      return 2800;
    case "F":
    case "V":
      return 1800;
    default:
      return 800;
  }
}

function writePhone(out: Float32Array, start: number, len: number, p: Phone) {
  if (p.kind === "gap") return;
  let noise = 0.17;
  for (let i = 0; i < len; i++) {
    const t = (start + i) / SAMPLE_RATE;
    const env = Math.sin(Math.PI * (i / len));
    let s = 0;
    if (p.kind === "vowel" || p.kind === "nasal") {
      const g = Math.sin(2 * Math.PI * 120 * t);
      s = g * (0.55 * Math.sin(2 * Math.PI * p.f1 * t) + 0.3 * Math.sin(2 * Math.PI * p.f2 * t) + 0.12 * Math.sin(2 * Math.PI * p.f3 * t));
      if (p.kind === "nasal") s *= 0.55;
    } else {
      noise = noise * 0.82 + (hash(start + i) - 0.5) * 0.35;
      const tone = p.f2 > 0 ? Math.sin(2 * Math.PI * p.f2 * t) : 1;
      s = noise * (p.kind === "burst" ? 1.4 : 0.9) * tone;
    }
    const at = start + i;
    if (at < out.length) out[at] += s * env * 0.35;
  }
}

function hash(n: number): number {
  const x = Math.sin(n * 12.9898) * 43758.5453;
  return x - Math.floor(x);
}
