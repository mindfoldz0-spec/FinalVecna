# FinalVecna — emergency call system (CERS/Vecnaverse v2)

Rust/Axum backend + Vite/React frontend. iOS-style caller dialer,
officer console, browser WebRTC calls, live SER emotion analysis
(self-hosted on AWS EC2), message STT (Sarvam primary / Gnani fallback)
+ Groq LLM triage.

## Run locally

```powershell
# backend (needs .env — see .env.example)
cd backend; cargo run
# frontend
cd frontend; npm install; npm run dev   # http://localhost:5173
```

Persistence locally: DynamoDB + S3 + SSM via the AWS CLI (see `aws/README.md`).

## Deploy on Render (free)

The repo root has a `render.yaml` Blueprint + `Dockerfile` (multi-stage:
builds `frontend/dist`, compiles Rust, serves everything from one service —
API, `/ws` WebSockets, and the static frontend).

1. **Dashboard** → **New** → **Blueprint** → connect this repo
   (`mindfoldz0-spec/FinalVecna`, `master` branch; or
   `mindfoldz0-spec/Hellfire_Mayureshv2.1`, `main` branch). No config needed —
   everything is in `render.yaml`.
2. Render prompts for the `sync: false` env vars — paste them from your
   **local `.env`** (never commit it). Same keys as `.env.example`, minus
   `DATABASE_URL` (Supabase is abandoned; nothing to set).
3. Render builds and deploys → live at `https://finalvecna.onrender.com`
   (caller: `/`, officer console: `/agent`).

Manual deploy (same thing): **New** → **Web Service** → repo → runtime
**Docker** → `Dockerfile Path: ./Dockerfile`, region **Singapore**,
plan **Free**, health check path `/api/health`, then add the same env vars in
**Environment**.

Notes:
- The free plan sleeps after ~15 min idle; first visit wakes it (~30–60 s).
- Every push to the branch auto-deploys (`autoDeployTrigger: commit`).
- The `EMOTION_ENDPOINT` is your EC2 SER worker. If the instance is
  stopped/started it gets a new public IP — update the env var and redeploy.
- Inside the Render container there is no AWS CLI, so DynamoDB/S3/SSM writes
  silently no-op and the app runs in-memory (great for demo). Locally it
  persists to your AWS account.
