# Supabase migration — Vecna emergency-call backend

**Project:** `fgluxpiotiuvnuznzady` (`https://fgluxpiotiuvnuznzady.supabase.co`), connected over MCP
(remote MCP server, OAuth as user Vecnaverse; config in `~/.config/opencode/opencode.json`).

## What changed (2026-11-06)

1. **Schema** (`0001_init` / `vecna_v2_init` applied via MCP `apply_migration`, mirrored in
   `backend/migrations/*.sql`): `agents, calls, recordings, transcripts, analyses, emotion_events,
   events` in `public`, TIMESTAMPTZ timestamps, BIGINT ids/co-ids, same CHECK/FK constraints as
   the old SQLite schema. **RLS enabled on all 7 tables with zero policies** — direct anon/REST
   access is denied; the backend connects as `postgres` (bypasses RLS). Advisory `rls_enabled_no_policy`
   (INFO ×7) is the expected state, not a finding to fix by adding policies.
2. **Backend is Postgres-only.** `db.rs` uses `sqlx::postgres::PgPool`
   (TLS required, statement cache 16, 5 connections, 30 s acquire timeout). Queries rewritten
   SQLite `?` → Postgres `$N`; `INSERT OR IGNORE` → `ON CONFLICT DO NOTHING`; reads of
   `created_at` etc. typed `chrono::DateTime<Utc>` and serialized to RFC3339 for the API.
   **SQLite is removed** — the server exits at boot if `DATABASE_URL` is not set.
   Local test data in `backend/vecna.db` was *not* migrated (e2e fixtures); delete it or move on.
3. **Connecting**: the backend expects the **Session pooler** string (port **5432** — prepared
   statements safe): `Dashboard → Connect → Session pooler`, e.g.
   `postgresql://postgres.fgluxpiotiuvnuznzady:[DB-PASSWORD]@aws-0-<region>.pooler.supabase.com:5432/postgres?sslmode=require`
   Put it in the repo-root `.env` as `DATABASE_URL=...` (never commit).
   **Do not use port 6543** (transaction mode — breaks sqlx prepared statements).
4. sqlx `migrate!` runs on boot; migration files are idempotent
   (`CREATE ... IF NOT EXISTS`, `ENABLE ROW LEVEL SECURITY`), so the first boot against the
   already-migrated project is a no-op.

## Verifying after connecting

```powershell
# DATABASE_URL set in .env, then:
.\start-server.ps1            # kills stale listener, rebuilds env, boots on :8000
```
Log must show `supabase postgres ready (migrations applied)`. Then:
- `curl http://localhost:8000/api/calls` → `{"calls":[...]}`
- Place a call at `/caller`, verify `/agent` queue, and re-run the e2e suites
  (`e2e-test/part2/disconnect/emotion/stt.mjs`) against the Supabase-backed server.
- MCP check: `list_tables` now shows row counts > 0.
