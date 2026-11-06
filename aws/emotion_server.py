"""Vecna emotion inference server.

Hosts the speech-emotion-recognition model and exposes a tiny stateless API:

  GET  /health                 -> {"ok": true, "model": ...} (200 only when model is loaded)
  POST /analyze                -> analyze audio
       body: raw PCM int16 LE (header X-Sample-Rate, default 16000)
       OR    multipart file field "audio" (wav/mp3/... via ffmpeg)
       header X-API-Key must match EMOTION_API_KEY
       response: {"label", "confidence", "scores": {label: prob}, "urgency", "duration_s"}

Run:  uvicorn emotion_server:app --host 0.0.0.0 --port 8080
"""
import io
import os

import numpy as np
import torch
from fastapi import FastAPI, Header, HTTPException, Request, UploadFile
from fastapi.responses import JSONResponse

MODEL_REPO = os.environ.get("EMOTION_MODEL_REPO", "Aniemore/wavlm-emotion-v1-crosslingual")
MODEL_DIR = os.environ.get("EMOTION_MODEL_DIR", "/opt/emotion-model")
MODEL_SUBFOLDER = os.environ.get("EMOTION_MODEL_SUBFOLDER", "") or None
API_KEY = os.environ.get("EMOTION_API_KEY", "")
SAMPLE_RATE = 16000
MIN_SECONDS = float(os.environ.get("EMOTION_MIN_SECONDS", "0.8"))
MAX_SECONDS = float(os.environ.get("EMOTION_MAX_SECONDS", "30.0"))

# Distress weight per emotion label -> queue urgency for the officer console.
URGENCY = {
    "fear": 3,
    "anger": 3,
    "sadness": 2,
    "disgust": 2,
    "neutral": 1,
    "happiness": 0,
    "enthusiasm": 0,
    "surprise": 1,
    "calm": 0,
}

app = FastAPI(title="vecna-emotion")
_state = {"model": None, "fe": None, "labels": []}


def load_model():
    from huggingface_hub import snapshot_download
    from transformers import AutoFeatureExtractor, AutoModelForAudioClassification

    path = MODEL_DIR
    if not (os.path.isdir(path) and os.listdir(path)):
        path = snapshot_download(MODEL_REPO, local_dir=MODEL_DIR)
    kw = {"subfolder": MODEL_SUBFOLDER} if MODEL_SUBFOLDER else {}
    fe = AutoFeatureExtractor.from_pretrained(path, **kw)
    model = AutoModelForAudioClassification.from_pretrained(path, **kw).eval()
    labels = [model.config.id2label[i] for i in range(model.config.num_labels)]
    _state["model"], _state["fe"], _state["labels"] = model, fe, labels


@app.on_event("startup")
def _startup():
    load_model()


def check_key(x_api_key: str | None):
    if API_KEY and x_api_key != API_KEY:
        raise HTTPException(status_code=401, detail="bad api key")


def decode_wav_bytes(data: bytes) -> np.ndarray:
    import soundfile as sf

    wav, sr = sf.read(io.BytesIO(data), dtype="float32", always_2d=False)
    if wav.ndim > 1:
        wav = wav.mean(axis=1)
    if sr != SAMPLE_RATE:
        import librosa

        wav = librosa.resample(wav, orig_sr=sr, target_sr=SAMPLE_RATE)
    return wav


def decode_any_file(data: bytes, name: str) -> np.ndarray:
    # non-wav (mp3/webm/ogg): go through ffmpeg
    import subprocess, tempfile

    with tempfile.NamedTemporaryFile(suffix=os.path.splitext(name)[1] or ".bin", delete=False) as f:
        f.write(data)
        tmp = f.name
    try:
        out = subprocess.run(
            ["ffmpeg", "-v", "error", "-i", tmp, "-ac", "1", "-ar", str(SAMPLE_RATE), "-f", "wav", "-"],
            capture_output=True,
            check=True,
        ).stdout
        return decode_wav_bytes(out)
    finally:
        os.unlink(tmp)


def run_inference(wav: np.ndarray) -> dict:
    model, fe = _state["model"], _state["fe"]
    dur = len(wav) / SAMPLE_RATE
    if dur < MIN_SECONDS:
        raise HTTPException(status_code=422, detail=f"audio too short ({dur:.2f}s < {MIN_SECONDS}s)")
    if dur > MAX_SECONDS:
        wav = wav[: int(MAX_SECONDS * SAMPLE_RATE)]
        dur = MAX_SECONDS
    x = fe(wav, sampling_rate=SAMPLE_RATE, return_tensors="pt", padding=True)
    with torch.no_grad():
        probs = model(**x).logits.softmax(-1)[0]
    scores = {lbl: round(float(p), 4) for lbl, p in zip(_state["labels"], probs)}
    label = max(scores, key=scores.get)
    return {
        "label": label,
        "confidence": scores[label],
        "scores": scores,
        "urgency": URGENCY.get(label, 1),
        "duration_s": round(dur, 2),
        "model": MODEL_REPO,
    }


@app.get("/health")
def health():
    if _state["model"] is None:
        return JSONResponse({"ok": False, "detail": "model not loaded"}, status_code=503)
    return {"ok": True, "model": MODEL_REPO, "labels": _state["labels"]}


@app.post("/analyze")
async def analyze(request: Request, x_api_key: str | None = Header(default=None), audio: UploadFile | None = None):
    check_key(x_api_key)
    if audio is not None:
        data = await audio.read()
        wav = decode_wav_bytes(data) if (audio.filename or "").lower().endswith((".wav", ".wave")) else decode_any_file(data, audio.filename or "audio.bin")
    else:
        raw = await request.body()
        sr = int(request.headers.get("x-sample-rate", SAMPLE_RATE))
        pcm = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0
        if sr != SAMPLE_RATE:
            import librosa

            pcm = librosa.resample(pcm, orig_sr=sr, target_sr=SAMPLE_RATE)
        wav = pcm
    return run_inference(wav)
