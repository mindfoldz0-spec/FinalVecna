# Emotion analysis worker (AWS)

Self-hosted speech emotion recognition (SER) for caller recordings, streamed
live to the officer console while a call waits in queue.

## Model choice

Primary model: **`Aniemore/wavlm-emotion-v1-crosslingual`** (int8 build, ~370 MB)

| candidate | why / why not |
|---|---|
| `Aniemore/wavlm-emotion-v1-crosslingual` (chosen) | WavLM-large backbone = multilingual (100+ languages incl. Hindi via pretraining; trained on 8 languages incl. Bengali + IEMOCAP + multilingual ASVP-ESD). 7 classes: anger, disgust, enthusiasm, fear, happiness, neutral, sadness. Released Aug 2026, strong spontaneous-speech scores. Plain `transformers`, easy to serve, int8 quant ships in-repo. |
| `emotion2vec/emotion2vec_plus_large` | SOTA on many 2024-2025 benchmarks (13 emotions), CPU-friendly, but FunASR runtime and Chinese/English-centric training. Good candidate as a second opinion / ensemble later. |
| `superb/hubert-large-superb-er` | English-only IEMOCAP SOTA (~80% acc, 4 classes). Wrong for Hindi/Hinglish callers. |
| `ehcalabres/wav2vec2-lg-xlsr-en-speech-emotion-recognition` | Popular English 7-class model; English-only. |
| AWS native (Transcribe Call Analytics) | Sentiment per utterance, not emotion — can be added later as a text-level complement. |

Labels map to a queue **urgency** score in `emotion_server.py`:
`fear/anger → 3`, `sadness/disgust → 2`, `neutral/surprise → 1`, `happy/enthusiastic/calm → 0`.

## Instance

- Account `930810028586` (user `Vecnaverse`), region **ap-southeast-2** (Sydney)
- Instance: `i-0c3e1faa76844b4ec`, type **c7i-flex.large** (free-tier eligible: 2 vCPU / 4 GB)
- Public IP: **3.25.119.144** (changes on stop/start — update `EMOTION_ENDPOINT` in `.env`)
- AMI: Ubuntu 24.04 (`ami-01e91e94accc42eec`), 40 GB gp3
- Security group `sg-0892d21ab5eeb4ecd`: SSH (22) and inference (8080) open **only to the operator's current public IP** (152.59.60.37/32). If your IP changes, re-authorize:
  `aws ec2 authorize-security-group-ingress --group-id sg-0892d21ab5eeb4ecd --protocol tcp --port 8080 --cidr <NEW_IP>/32`
  (and remove the old rule)
- SSH key: `C:\Users\mayur\.ssh\vecna-emotion.pem` (key pair `vecna-emotion`)
- Model weights live on the instance at `/opt/emotion-model` (int8), server at `/opt/emotion_server.py`, systemd unit `emotion.service` on port **8080**

Manage:
```
aws ec2 start-instances  --instance-ids i-0c3e1faa76844b4ec
aws ec2 stop-instances   --instance-ids i-0c3e1faa76844b4ec   # stop when not testing to avoid charges
aws ec2 describe-instances --instance-ids i-0c3e1faa76844b4ec --query "Reservations[0].Instances[0].PublicIpAddress"
ssh -i ~/.ssh/vecna-emotion.pem ubuntu@3.25.119.144
```

Rebuild from scratch: `aws/user-data.template.sh` (placeholders `__SERVER_B64__`, `__API_KEY__`
are filled at launch time — never commit the generated file).

## Inference API

```
GET  /health                     -> {"ok":true,"model":...,"labels":[...]}   (503 until model loaded)
POST /analyze  X-API-Key: ...    -> body: raw PCM int16 LE (X-Sample-Rate, default 16000)
                                      OR multipart file "audio" (wav/mp3/webm via ffmpeg)
                   <- {"label","confidence","scores":{...},"urgency","duration_s","model"}
```

API key is `EMOTION_API_KEY` in the repo `.env` (gitignored). Test from the operator machine:
```
curl http://3.25.119.144:8080/health
curl -X POST http://3.25.119.144:8080/analyze -H "X-API-Key: $EMOTION_API_KEY" --data-binary @test.pcm -H "X-Sample-Rate: 16000"
```

## Real-time pipeline (implemented)

1. **Caller** (`frontend/src/lib/pcmStream.ts` + `Caller.tsx`): after the beep, an AudioWorklet
   taps the mic and streams ~1 s chunks of 16 kHz mono int16 PCM as **binary WS frames** on the
   existing caller socket. Runs through the hold phase; stops when an officer accepts
   (`AGENT_ASSIGNED`) or the call ends, resumes if the call is released back to the queue.
2. **Backend** (`backend/src/emotion.rs`, wired in `signaling.rs`): buffers PCM per call; every
   ~2 s of new audio POSTs the window (max 8 s) to `EMOTION_ENDPOINT/analyze` with
   `EMOTION_API_KEY`; EMA-smooths scores; broadcasts `EMOTION_UPDATE` to all agent sockets;
   stores rows in `emotion_events` (migration `0002_emotions.sql`) and caches the latest reading
   on the in-memory call (`Call.emotion`, included in NEW_CALL/STATUS_CHANGED/CLAIM_RESULT).
   Live analysis only runs while the call is RINGING/WAITING — it stops at claim/end.
3. **Officer console** (`Agent.tsx`): queue cards show a live emotion chip
   (red=3 / orange=2 / gray=1 / green=0, label + confidence, full scores on hover);
   the queue sorts by urgency desc, then oldest first, so the officer sees the most
   distressed caller on top and picks accordingly. The active-call panel shows the chip too.
4. **Final analysis**: when the caller's message recording is uploaded
   (`POST /api/recordings/:id`, side `message`), the backend POSTs the file to `/analyze`
   in a spawned task → stored as `source='final'` in `emotion_events` **and** as an
   `analyses` row (kind='emotion', provider='ec2-ser') linked to the recording; broadcast
   as `EMOTION_UPDATE source='final'`. Inspect via `GET /api/calls/:id` (`emotions`, `analyses`).

WS protocol additions: caller→server **binary frames** = raw PCM int16 LE @16 kHz;
server→agents `{"event":"EMOTION_UPDATE","call_id","label","confidence","urgency","scores","source"}`;
`CLAIM_RESULT`/`CALL_CLAIMED`/`AGENT_ASSIGNED`/`CallInfo` may carry `"emotion"`.

E2E: `e2e-emotion.mjs` (in the opencode temp dir) streams a real speech sample as PCM and
asserts live updates while RINGING and WAITING, claim payload, final analysis, and DB rows.

## Speech-to-text + text triage of the message (implemented)

When the caller's message recording is uploaded (`POST /api/recordings/:id`), the backend
spawns `analysis::run_message_analysis` (upload response never waits on STT/LLM):

1. **STT — Sarvam primary, Gnani fallback only** (`backend/src/analysis.rs`):
   - Sarvam `POST https://api.sarvam.ai/speech-to-text` (`api-subscription-key` header,
     multipart `file` + `model=saaras:v4` + `language_code=unknown`) on the webm message.
   - Gnani `POST https://api.vachana.ai/stt/v3` (`X-API-Key-ID`, `audio_file` +
     `model=prisma-v3.0` + `language_code=indic-auto` + `format=transcribe`) runs **only**
     if Sarvam errors/times out/returns empty — never alternated. Gnani can't read webm,
     so the caller also uploads a `message_wav` WAV sidecar built from the same live PCM
     stream (`pcmToWav` in `frontend/src/lib/pcmStream.ts`, accumulated in `Caller.tsx`
     only while `phase === 'recording'`).
   - Transcript stored in `transcripts` (provider `sarvam` | `gnani`) + `TRANSCRIPT` event.
2. **Triage — Qwen on Groq**: transcript → `POST https://api.groq.com/openai/v1/chat/completions`
   with model `qwen/qwen3.8-27b` (override via `GROQ_MODEL`), `response_format: json_object`.
   Returns `{category, label, urgency 0-3, summary, keywords[], flags[]}` where category is one of
   `cybercrime-financial | cybercrime-fraud-impersonation | cybercrime-data-privacy |
   image-based-abuse | cybercrime-other | harassment | physical-crime | accident-medical |
   non-crime | unclear` (image-based-abuse covers "more photos"/morphed-image blackmail; flags
   include `csam`, `minor`, `immediate-danger`, `financial-loss`, `ongoing`). Stored in
   `analyses` (kind='triage', provider='groq'), cached on the call (`Call.triage`), and
   broadcast to all officers as `TRIAGE_UPDATE`.
3. **Officer queue** (`Agent.tsx`): each card now shows an urgency badge `U0..U3`
   (effective urgency = max(emotion, triage)) and a triage panel with the category chip,
   `URGENCY x/3`, the short label, one-line summary, and red flag badges. Queue sorts by
   effective urgency desc. `GET /api/calls` exposes `last_category` / `last_triage_urgency`;
   `GET /api/calls/:id` returns full `transcripts` + `analyses`.

Keys: env `SARVAM_API_KEY` / `GNANI_API_KEY` / `GROQ_API_KEY` / `GROQ_MODEL`, with a manual
fallback that parses the repo-root `.env` colon-format lines (`sarvav:…`, `gnani:…`, `groq:…`)
which dotenvy does not load.

E2E: `e2e-stt.mjs [baseUrl]` — uploads `test.wav` as `message` + `message_wav`, asserts
`TRIAGE_UPDATE`, transcript + triage DB rows, and list fields. Primary path (Sarvam) passes on
:8000; run a second instance with `PORT=8001 SARVAM_API_KEY=bogus DATABASE_PATH=…` to prove the
Gnani fallback (`transcripts.provider = 'gnani'`).
