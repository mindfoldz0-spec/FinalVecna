// Tap a mic MediaStream into 16 kHz mono int16 PCM chunks (binary WS frames)
// for the live emotion analyzer on the server. ~1 s per chunk.

const WORKLET_CODE = `
class PcmTap extends AudioWorkletProcessor {
  constructor() {
    super();
    this.out = [];
    this.ratio = sampleRate / 16000; // decimation factor (e.g. 3 at 48 kHz)
    this.pos = 0;
  }
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    if (ch) {
      for (let i = 0; i < ch.length; i++) {
        this.pos += this.ratio;
        if (this.pos >= 1) {
          this.pos -= 1;
          const y = Math.max(-1, Math.min(1, ch[i]));
          this.out.push(Math.round(y * 32767));
        }
      }
      if (this.out.length >= 16000) { // ~1 s at 16 kHz
        const n = this.out.length;
        const buf = new ArrayBuffer(n * 2);
        const v = new DataView(buf);
        for (let j = 0; j < n; j++) v.setInt16(j * 2, this.out[j], true);
        this.out = [];
        this.port.postMessage(buf, [buf]);
      }
    }
    return true;
  }
}
registerProcessor('pcm-tap', PcmTap);
`;

/** Pack raw 16 kHz mono int16 PCM chunks into a standard WAV file blob
 *  (44-byte header). Used for the message_wav sidecar upload — Gnani STT
 *  can't read webm, so it needs a WAV of the same audio. */
export function pcmToWav(chunks: ArrayBuffer[], sampleRate = 16000): Blob {
  let len = 0;
  for (const c of chunks) len += c.byteLength;
  const buf = new ArrayBuffer(44 + len);
  const v = new DataView(buf);
  const writeStr = (off: number, s: string) => {
    for (let i = 0; i < s.length; i++) v.setUint8(off + i, s.charCodeAt(i));
  };
  writeStr(0, 'RIFF');
  v.setUint32(4, 36 + len, true);
  writeStr(8, 'WAVE');
  writeStr(12, 'fmt ');
  v.setUint32(16, 16, true);          // PCM fmt chunk size
  v.setUint16(20, 1, true);           // PCM format
  v.setUint16(22, 1, true);           // mono
  v.setUint32(24, sampleRate, true);
  v.setUint32(28, sampleRate * 2, true); // byte rate
  v.setUint16(32, 2, true);           // block align
  v.setUint16(34, 16, true);          // bits per sample
  writeStr(36, 'data');
  v.setUint32(40, len, true);
  let off = 44;
  for (const c of chunks) {
    new Uint8Array(buf, off, c.byteLength).set(new Uint8Array(c));
    off += c.byteLength;
  }
  return new Blob([buf], { type: 'audio/wav' });
}

/** Start streaming PCM chunks. Returns a stop function. */
export function startPcmStream(
  stream: MediaStream,
  onChunk: (pcm: ArrayBuffer) => void,
): () => void {
  let stopped = false;
  let ctx: AudioContext | null = null;
  let node: AudioWorkletNode | null = null;
  const url = URL.createObjectURL(new Blob([WORKLET_CODE], { type: 'application/javascript' }));
  (async () => {
    try {
      const Ctx =
        window.AudioContext ??
        (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
      ctx = new Ctx();
      await ctx.audioWorklet.addModule(url);
      if (stopped) {
        ctx.close().catch(() => {});
        return;
      }
      const src = ctx.createMediaStreamSource(stream);
      node = new AudioWorkletNode(ctx, 'pcm-tap');
      node.port.onmessage = (e: MessageEvent<ArrayBuffer>) => onChunk(e.data);
      src.connect(node);
    } catch (e) {
      console.warn('[pcm] failed to start stream', e);
    }
  })();
  return () => {
    stopped = true;
    try { node?.disconnect(); } catch { /* ignore */ }
    try { ctx?.close().catch(() => {}); } catch { /* ignore */ }
    URL.revokeObjectURL(url);
  };
}
