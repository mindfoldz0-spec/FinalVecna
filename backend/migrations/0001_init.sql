-- FinalVecna v2 schema — Supabase Postgres (public schema).
-- Applied via MCP apply_migration vecna_v2_init; sqlx keeps this file in sync.

CREATE TABLE IF NOT EXISTS public.agents (
    agent_id      TEXT PRIMARY KEY,
    display_name  TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    is_active     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_login_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS public.calls (
    call_id          TEXT PRIMARY KEY,
    dialed_number    TEXT NOT NULL,
    status           TEXT NOT NULL CHECK (status IN ('RINGING','WAITING','ENGAGED','ENDED')),
    agent_id         TEXT REFERENCES public.agents(agent_id),
    caller_ip        TEXT,
    ivr_finished_at  TIMESTAMPTZ,
    claimed_at       TIMESTAMPTZ,
    engaged_at       TIMESTAMPTZ,
    ended_at         TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_calls_status  ON public.calls(status);
CREATE INDEX IF NOT EXISTS idx_calls_created ON public.calls(created_at);
CREATE INDEX IF NOT EXISTS idx_calls_agent   ON public.calls(agent_id);

CREATE TABLE IF NOT EXISTS public.recordings (
    recording_id TEXT PRIMARY KEY,
    call_id      TEXT NOT NULL REFERENCES public.calls(call_id) ON DELETE CASCADE,
    side         TEXT NOT NULL CHECK (side IN ('message','caller','agent','system')),
    filename     TEXT NOT NULL,
    bytes        BIGINT NOT NULL,
    mime         TEXT NOT NULL,
    duration_ms  BIGINT,
    rms          DOUBLE PRECISION,
    zcr          DOUBLE PRECISION,
    storage      TEXT NOT NULL DEFAULT 'local',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_recordings_call ON public.recordings(call_id);

CREATE TABLE IF NOT EXISTS public.transcripts (
    transcript_id TEXT PRIMARY KEY,
    recording_id  TEXT NOT NULL REFERENCES public.recordings(recording_id) ON DELETE CASCADE,
    language      TEXT,
    text          TEXT NOT NULL,
    confidence    DOUBLE PRECISION,
    provider      TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS public.analyses (
    analysis_id  TEXT PRIMARY KEY,
    recording_id TEXT NOT NULL REFERENCES public.recordings(recording_id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN ('emotion','sentiment','intent','summary','triage')),
    provider     TEXT,
    model        TEXT,
    result_json  TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_analyses_recording ON public.analyses(recording_id);

CREATE TABLE IF NOT EXISTS public.events (
    event_id   TEXT PRIMARY KEY,
    call_id    TEXT REFERENCES public.calls(call_id) ON DELETE CASCADE,
    agent_id   TEXT,
    event      TEXT NOT NULL,
    detail_json TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_events_call ON public.events(call_id);

ALTER TABLE public.agents     ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.calls      ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.recordings ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.transcripts ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.analyses   ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.events     ENABLE ROW LEVEL SECURITY;
-- No policies: backend connects as postgres (BYPASSRLS); anon/authenticated get nothing.
