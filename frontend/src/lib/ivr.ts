// IVR helpers: one-shot/looped audio playback + mic silence detection
// for the "speak your message after the beep" window.

/** Play a file once. Resolves with the element on 'ended' (or when maxPlayMs
 *  elapses — the element is paused then, so the caller can move on immediately),
 *  null on error/404. If the browser blocks autoplay, waits for the next
 *  tap/keypress and retries inside that gesture (Chrome's transient activation
 *  expires during the async getUserMedia/fetch/WS setup).
 *  onStart, if given, is called synchronously with the element so callers can
 *  register it BEFORE it plays (e.g. so a mid-clip hangup can pause it). */
export function playOnce(url: string, timeoutMs = 30000, maxPlayMs?: number, onStart?: (a: HTMLAudioElement) => void): Promise<HTMLAudioElement | null> {
  return new Promise((resolve) => {
    const a = new Audio(url);
    a.preload = 'auto';
    onStart?.(a);
    console.log('[ivr] play', url);
    let settled = false;
    const done = (v: HTMLAudioElement | null) => {
      if (!settled) { settled = true; resolve(v); }
    };
    a.onended = () => { console.log('[ivr] ended', url); done(a); };
    a.onerror = () => { console.error('[ivr] error loading', url, a.error?.message); done(null); };
    // never hang the sequence — and actually stop the clip, so a slow load
    // can't overlap the next voice
    setTimeout(() => { try { a.pause(); } catch { /* ignore */ } console.warn('[ivr] timeout', url); done(a); }, timeoutMs);
    if (maxPlayMs) {
      setTimeout(() => { try { a.pause(); } catch { /* ignore */ } console.log('[ivr] capped', url); done(a); }, maxPlayMs);
    }
    a.play().catch((e: DOMException) => {
      if (e?.name === 'NotAllowedError') {
        console.warn('[ivr] autoplay blocked, will start on next tap', url);
        const retry = () => {
          window.removeEventListener('pointerdown', retry);
          window.removeEventListener('keydown', retry);
          // stopped meanwhile (e.g. caller hung up) — never resurrect the clip
          if (a.dataset.dead) return;
          a.play().catch((e2) => { console.error('[ivr] retry failed', url, e2); done(null); });
        };
        window.addEventListener('pointerdown', retry);
        window.addEventListener('keydown', retry);
      } else {
        console.error('[ivr] play failed', url, e);
        done(null);
      }
    });
  });
}

/** Loop the first URL that loads; falls back to the next on error. */
export function playLoopFirst(urls: string[]): HTMLAudioElement {
  let i = 0;
  const a = new Audio(urls[0]);
  a.loop = true;
  console.log('[ivr] loop', urls[0]);
  a.onerror = () => {
    if (a.dataset.dead) return;
    if (i + 1 < urls.length) {
      i += 1;
      console.warn('[ivr] loop fallback ->', urls[i]);
      a.src = urls[i];
      a.play().catch((e) => console.error('[ivr] loop play failed', urls[i], e));
    } else {
      console.error('[ivr] all loop urls failed');
    }
  };
  a.play().catch((e: DOMException) => {
    if (e?.name === 'NotAllowedError') {
      console.warn('[ivr] loop autoplay blocked, will start on next tap', urls[0]);
      const retry = () => {
        window.removeEventListener('pointerdown', retry);
        window.removeEventListener('keydown', retry);
        if (a.dataset.dead) return;
        a.play().catch((e2) => console.error('[ivr] loop retry failed', urls[i], e2));
      };
      window.addEventListener('pointerdown', retry);
      window.addEventListener('keydown', retry);
    } else {
      console.error('[ivr] loop play failed', urls[0], e);
    }
  });
  return a;
}

/** Try URLs in order; play the first one that exists/plays. maxPlayMs caps how
 *  long the first successful file is allowed to run before moving on. */
export async function playFirst(urls: string[], timeoutMs = 20000, maxPlayMs?: number, onStart?: (a: HTMLAudioElement) => void): Promise<HTMLAudioElement | null> {
  for (const u of urls) {
    const a = await playOnce(u, timeoutMs, maxPlayMs, onStart);
    if (a) return a;
  }
  return null;
}

export function stopAudio(a: HTMLAudioElement | null | undefined) {
  if (!a) return;
  // kill-switch: armed tap-to-unlock retries must never resurrect this clip
  a.dataset.dead = '1';
  try { a.pause(); } catch { /* ignore */ }
  a.src = '';
}

export interface SilenceOpts {
  threshold?: number;   // RMS above this = speech
  silenceMs?: number;   // sustained silence after speech => done (default 2200)
  minMs?: number;       // message must run at least this long before silence can end it (default 3000)
  maxMs?: number;       // hard safety cap for the message window (default 60000)
  onLevel?: (v: number) => void; // 0..1 for the meter
  onDone: () => void;
}

/** Watch the mic stream; call onDone when the caller stops speaking for
 *  silenceMs (but not before minMs of recording), or at the maxMs cap. */
export function startSilenceWatcher(stream: MediaStream, opts: SilenceOpts): () => void {
  const ctx = new AudioContext();
  const src = ctx.createMediaStreamSource(stream);
  const an = ctx.createAnalyser();
  an.fftSize = 512;
  src.connect(an);
  const buf = new Float32Array(an.fftSize);
  const threshold = opts.threshold ?? 0.02;
  const silenceMs = opts.silenceMs ?? 2200;
  const minMs = opts.minMs ?? 3000;
  const maxMs = opts.maxMs ?? 60000;
  const t0 = performance.now();
  let lastLoud = 0; // ms since t0 when speech was last heard (0 = start)
  let done = false;
  const finish = () => {
    if (done) return;
    done = true;
    clearInterval(iv);
    ctx.close().catch(() => {});
    opts.onDone();
  };
  const iv = setInterval(() => {
    an.getFloatTimeDomainData(buf);
    let sum = 0;
    for (let i = 0; i < buf.length; i++) sum += buf[i] * buf[i];
    const rms = Math.sqrt(sum / buf.length);
    opts.onLevel?.(Math.min(1, rms * 4));
    const t = performance.now() - t0;
    if (rms > threshold) lastLoud = t;
    const silentFor = t - lastLoud;
    if ((silentFor >= silenceMs && t >= minMs) || t >= maxMs) return finish();
  }, 100);
  return finish;
}
