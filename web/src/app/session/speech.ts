import { signal } from '@angular/core';
import { Server } from '../settings';

/** rho's `/tts` audio: 16 bit little endian PCM, mono. */
const RATE = 24000;

/**
 * Reads a text aloud with rho's `POST /tts`. While the audio streams in, Web
 * Audio plays the chunks; once all is there, `url` holds it as a WAV file, so
 * an `<audio>` element can take over with seeking and replay.
 */
export class Speech {
  readonly error = signal<string | undefined>(undefined);
  /** Seconds of audio received so far. */
  readonly received = signal(0);
  /** The whole audio as a blob URL, once the stream is done. */
  readonly url = signal<string | undefined>(undefined);

  private readonly ctx = new AudioContext();
  private readonly abort = new AbortController();
  private start = 0;
  private next = 0;

  /** Call from a click: browsers only let a user gesture start audio. */
  constructor(server: Server, text: string) {
    void this.run(server, text);
  }

  /** Where the streamed playback is, in seconds. */
  position(): number {
    return this.start ? Math.max(0, this.ctx.currentTime - this.start) : 0;
  }

  /** Ends the streamed playback, e.g. once the `<audio>` element plays. */
  stopStream() {
    void this.ctx.close().catch(() => {});
  }

  close() {
    this.abort.abort();
    this.stopStream();
    const url = this.url();
    if (url) URL.revokeObjectURL(url);
  }

  private async run(server: Server, text: string) {
    try {
      const res = await fetch(new URL('/tts', server.url.replace(/^ws/, 'http')), {
        method: 'POST',
        headers: { Authorization: `Bearer ${server.token}` },
        body: text,
        signal: this.abort.signal,
      });
      if (!res.ok || !res.body) throw new Error((await res.text()) || res.statusText);
      const chunks: Uint8Array[] = [];
      let bytes = 0;
      let odd: Uint8Array | undefined;
      const reader = res.body.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        chunks.push(value);
        bytes += value.length;
        this.received.set(bytes / 2 / RATE);
        // A sample can be split between two reads.
        const data = odd ? concat([odd, value]) : value;
        const even = data.length & ~1;
        odd = even < data.length ? data.slice(even) : undefined;
        this.play(data.subarray(0, even));
      }
      this.url.set(URL.createObjectURL(new Blob([wav(concat(chunks))], { type: 'audio/wav' })));
    } catch (e) {
      if (this.abort.signal.aborted) return;
      this.error.set(e instanceof Error ? e.message : String(e));
    }
  }

  private play(pcm: Uint8Array) {
    if (!pcm.length || this.ctx.state === 'closed') return;
    const samples = floats(pcm);
    const buffer = this.ctx.createBuffer(1, samples.length, RATE);
    buffer.copyToChannel(samples, 0);
    const source = this.ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(this.ctx.destination);
    // shortcut: a stall in the stream shifts `position` by the gap; fine while Gemini is faster than real time.
    if (!this.start) this.next = this.start = this.ctx.currentTime + 0.05;
    this.next = Math.max(this.next, this.ctx.currentTime);
    source.start(this.next);
    this.next += buffer.duration;
  }
}

/** 16 bit little endian samples as floats. */
export function floats(pcm: Uint8Array): Float32Array<ArrayBuffer> {
  const view = new DataView(pcm.buffer, pcm.byteOffset, pcm.length);
  const out = new Float32Array(pcm.length >> 1);
  for (let i = 0; i < out.length; i++) out[i] = view.getInt16(i * 2, true) / 32768;
  return out;
}

/** The PCM with a WAV header in front. */
export function wav(pcm: Uint8Array): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(44 + pcm.length);
  const view = new DataView(out.buffer);
  const text = (at: number, s: string) =>
    [...s].forEach((c, i) => view.setUint8(at + i, c.charCodeAt(0)));
  text(0, 'RIFF');
  view.setUint32(4, 36 + pcm.length, true);
  text(8, 'WAVEfmt ');
  view.setUint32(16, 16, true); // fmt chunk size
  view.setUint16(20, 1, true); // PCM
  view.setUint16(22, 1, true); // mono
  view.setUint32(24, RATE, true);
  view.setUint32(28, RATE * 2, true); // bytes per second
  view.setUint16(32, 2, true); // bytes per frame
  view.setUint16(34, 16, true); // bits per sample
  text(36, 'data');
  view.setUint32(40, pcm.length, true);
  out.set(pcm, 44);
  return out;
}

function concat(parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}
