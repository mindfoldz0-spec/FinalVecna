// Simple in-call mic recorder -> POSTs to /api/recordings/{call_id}.

export function startCallRecorder(stream: MediaStream): MediaRecorder | null {
  try {
    const mime = MediaRecorder.isTypeSupported('audio/webm;codecs=opus')
      ? 'audio/webm;codecs=opus'
      : '';
    const rec = new MediaRecorder(stream, mime ? { mimeType: mime } : undefined);
    const chunks: Blob[] = [];
    rec.ondataavailable = (e) => {
      if (e.data.size > 0) chunks.push(e.data);
    };
    rec.start(1000);
    (rec as unknown as { _chunks: Blob[] })._chunks = chunks;
    return rec;
  } catch {
    return null;
  }
}

function recorderBlob(rec: MediaRecorder): Blob | null {
  const chunks = (rec as unknown as { _chunks?: Blob[] })._chunks ?? [];
  if (!chunks.length) return null;
  const blob = new Blob(chunks, { type: rec.mimeType || 'audio/webm' });
  return blob.size > 0 ? blob : null;
}

export async function stopAndUpload(
  rec: MediaRecorder | null,
  callId: string,
  callToken: string,
  field = 'caller_audio',
  extra?: Array<[string, Blob, string]>,
): Promise<void> {
  if (!rec || rec.state === 'inactive') return;
  const blob: Blob | null = await new Promise((resolve) => {
    rec.onstop = () => resolve(recorderBlob(rec));
    try {
      rec.stop();
    } catch {
      resolve(null);
    }
  });
  if (!blob) return;
  const ext = blob.type.includes('wav') ? 'wav' : 'webm';
  const form = new FormData();
  form.append(field, blob, `recording_${callId}.${ext}`);
  for (const [name, b, fname] of extra ?? []) form.append(name, b, fname);
  const ctrl = new AbortController();
  const t = setTimeout(() => ctrl.abort(), 15000); // never stall the call flow on a hung upload
  try {
    const res = await fetch(`/api/recordings/${encodeURIComponent(callId)}`, {
      method: 'POST',
      headers: { 'X-Call-Token': callToken },
      body: form,
      signal: ctrl.signal,
    });
    if (!res.ok) console.warn('[rec] upload rejected', res.status);
  } catch (e) {
    console.warn('[rec] upload failed', e);
  } finally {
    clearTimeout(t);
  }
}
