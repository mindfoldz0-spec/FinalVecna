-- Live + final speech-emotion results (caller side).
CREATE TABLE IF NOT EXISTS public.emotion_events (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    call_id      TEXT NOT NULL REFERENCES public.calls(call_id) ON DELETE CASCADE,
    label        TEXT NOT NULL,
    confidence   DOUBLE PRECISION NOT NULL,
    scores_json  TEXT NOT NULL,
    urgency      BIGINT NOT NULL,
    source       TEXT NOT NULL CHECK (source IN ('live','final')),
    recording_id TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_emotion_events_call ON public.emotion_events(call_id, id);
ALTER TABLE public.emotion_events ENABLE ROW LEVEL SECURITY;
