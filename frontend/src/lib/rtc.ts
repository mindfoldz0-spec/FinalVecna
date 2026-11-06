// Shared WebRTC helper. Caller creates the OFFER (standard), agent answers.

let rtcConfigPromise: Promise<RTCConfiguration> | null = null;

export async function getRtcConfig(): Promise<RTCConfiguration> {
  if (!rtcConfigPromise) {
    rtcConfigPromise = fetch('/api/rtc-config')
      .then((r) => (r.ok ? r.json() : null))
      .then((cfg) => {
        if (cfg?.iceServers?.length) return { iceServers: cfg.iceServers } as RTCConfiguration;
        throw new Error('no config');
      })
      .catch(() => ({
        iceServers: [
          { urls: 'stun:stun.l.google.com:19302' },
          { urls: 'stun:stun1.l.google.com:19302' },
        ],
      }));
  }
  return rtcConfigPromise;
}

export interface PeerOpts {
  localStream: MediaStream | null;
  onRemoteStream: (stream: MediaStream) => void;
  onSignal: (payload: RTCSessionDescriptionInit | { candidate: RTCIceCandidateInit }) => void;
  onStateChange?: (state: RTCPeerConnectionState) => void;
  /** Called when ICE fails — the offerer should re-offer with iceRestart. */
  onIceFailed?: () => void;
}

export async function createPeer(opts: PeerOpts): Promise<RTCPeerConnection> {
  const pc = new RTCPeerConnection(await getRtcConfig());

  if (opts.localStream) {
    for (const track of opts.localStream.getTracks()) {
      pc.addTrack(track, opts.localStream);
    }
  }

  pc.onicecandidate = (e) => {
    if (e.candidate) opts.onSignal({ candidate: e.candidate.toJSON() });
  };
  pc.ontrack = (e) => {
    if (e.streams[0]) opts.onRemoteStream(e.streams[0]);
  };
  pc.onconnectionstatechange = () => opts.onStateChange?.(pc.connectionState);
  pc.oniceconnectionstatechange = () => {
    if (pc.iceConnectionState === 'failed') opts.onIceFailed?.();
  };

  return pc;
}

export async function flushCandidates(pc: RTCPeerConnection, candidates: RTCIceCandidateInit[]) {
  for (const c of candidates) {
    try {
      await pc.addIceCandidate(new RTCIceCandidate(c));
    } catch (e) {
      console.error('[rtc] bad ICE candidate', e);
    }
  }
}

export async function applyRemote(
  pc: RTCPeerConnection,
  payload: { offer?: RTCSessionDescriptionInit; answer?: RTCSessionDescriptionInit; candidate?: RTCIceCandidateInit } & { type?: string; sdp?: string },
  buffered: RTCIceCandidateInit[],
): Promise<{ answer?: RTCSessionDescriptionInit }> {
  // Tolerate both wrapped ({ answer: {...} }) and raw ({ type: 'answer', sdp }) shapes.
  let norm = payload as { offer?: RTCSessionDescriptionInit; answer?: RTCSessionDescriptionInit; candidate?: RTCIceCandidateInit };
  if (payload && typeof (payload as { type?: unknown }).type === 'string' && typeof (payload as { sdp?: unknown }).sdp === 'string') {
    const t = (payload as { type: string }).type;
    if (t === 'offer') norm = { offer: payload as unknown as RTCSessionDescriptionInit };
    else if (t === 'answer') norm = { answer: payload as unknown as RTCSessionDescriptionInit };
  }
  if (norm.answer) {
    if (pc.signalingState !== 'have-local-offer') return {}; // stray/duplicate answer
    await pc.setRemoteDescription(new RTCSessionDescription(norm.answer));
    await flushCandidates(pc, buffered.splice(0));
    return {};
  }
  if (norm.offer) {
    // Glare guard: if we already have a local offer, prefer the caller's and restart ours.
    if (pc.signalingState !== 'stable') {
      try { await pc.setLocalDescription({ type: 'rollback' }); } catch { /* ignore */ }
    }
    await pc.setRemoteDescription(new RTCSessionDescription(norm.offer));
    await flushCandidates(pc, buffered.splice(0));
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);
    return { answer: pc.localDescription!.toJSON() };
  }
  const cand = norm.candidate ?? (payload as { candidate?: RTCIceCandidateInit }).candidate;
  if (cand) {
    const init = (cand as { candidate?: unknown }).candidate && typeof (cand as { candidate: unknown }).candidate === 'object'
      ? (cand as unknown as RTCIceCandidateInit)
      : (cand as RTCIceCandidateInit);
    // Raw RTCIceCandidateInit passed directly
    const real = (payload as { candidate?: RTCIceCandidateInit }).candidate ? init : (payload as unknown as RTCIceCandidateInit);
    if (!pc.remoteDescription) buffered.push(real);
    else {
      try {
        await pc.addIceCandidate(new RTCIceCandidate(real));
      } catch (e) {
        console.error('[rtc] bad ICE candidate', e);
      }
    }
  }
  return {};
}
