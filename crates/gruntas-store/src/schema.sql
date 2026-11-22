-- 001_init.sql is the whole v1 schema story: applied once, in one
-- transaction, on a fresh file, then PRAGMA user_version is set to 1.
-- Open never runs an in-place ALTER in v1 and there is no migrations
-- table until Phase 2 (the first phase that writes state the source
-- files do not hold). A version or shape mismatch fails Open with a
-- rebuild hint instead of a silent repair.
--
-- Names are load-bearing: worktree, never project or workspace. Every id
-- is TEXT PRIMARY KEY holding a uuidv7 minted by this package (see
-- ValidateID); the one table without an id is import_state, keyed by
-- source instead. seq is the only ordering key: transcripts do not keep
-- time straight, so order of record is a sequence, not a clock. Dedup is
-- a unique index on foreign ids, never an id derived from them — a
-- second import of the same Claude session converges on one row because
-- the (provider, foreign_session_id) index says so.
--
-- Conventions every table follows (pinned by schema_conventions_test):
-- every *_id column has a FOREIGN KEY with ON DELETE CASCADE, every FK
-- child column leads an index, every time_* column is INTEGER
-- milliseconds since the Unix epoch (UTC), role is a closed CHECK while
-- provider/harness/type stay open strings.

CREATE TABLE worktree (
  id TEXT PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,
  vcs TEXT NOT NULL DEFAULT '',
  name TEXT NOT NULL DEFAULT '',
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL
);

CREATE TABLE session (
  id TEXT PRIMARY KEY,
  worktree_id TEXT NOT NULL,
  parent_id TEXT NULL,
  directory TEXT NOT NULL DEFAULT '',
  title TEXT NOT NULL DEFAULT '',
  model TEXT NOT NULL DEFAULT '',
  provider TEXT NOT NULL DEFAULT '',
  harness TEXT NOT NULL DEFAULT '',
  revision INTEGER NOT NULL DEFAULT 1,
  remote_slug TEXT NULL UNIQUE,
  deleted_at INTEGER NULL,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  FOREIGN KEY (worktree_id) REFERENCES worktree(id) ON DELETE CASCADE,
  FOREIGN KEY (parent_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE TABLE session_binding (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  provider TEXT NOT NULL DEFAULT '',
  harness TEXT NOT NULL DEFAULT '',
  foreign_session_id TEXT NOT NULL,
  resume_cmd TEXT NOT NULL DEFAULT '',
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE,
  UNIQUE (provider, foreign_session_id)
);

CREATE TABLE session_event (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  type TEXT NOT NULL DEFAULT '',
  data TEXT NOT NULL DEFAULT '{}',
  time_created INTEGER NOT NULL,
  FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE,
  UNIQUE (session_id, seq)
);

CREATE TABLE turn (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT '',
  cost_input_tokens INTEGER NOT NULL DEFAULT 0,
  cost_output_tokens INTEGER NOT NULL DEFAULT 0,
  cost_total_tokens INTEGER NOT NULL DEFAULT 0,
  cost_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  cost_cache_write_tokens INTEGER NOT NULL DEFAULT 0,
  cost_reasoning_tokens INTEGER NOT NULL DEFAULT 0,
  cost_usd_micros INTEGER NULL,
  remote_slug TEXT NULL UNIQUE,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE,
  UNIQUE (session_id, seq)
);

CREATE TABLE message (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  turn_id TEXT NULL,
  seq INTEGER NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'system', 'tool', 'error')),
  provider TEXT NOT NULL DEFAULT '',
  model TEXT NOT NULL DEFAULT '',
  foreign_id TEXT NULL,
  usage TEXT NOT NULL DEFAULT '{}',
  raw_json TEXT NULL,
  time_created INTEGER NOT NULL,
  FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE,
  FOREIGN KEY (turn_id) REFERENCES turn(id) ON DELETE CASCADE,
  UNIQUE (session_id, seq)
);

CREATE UNIQUE INDEX idx_message_session_foreign ON message(session_id, foreign_id) WHERE foreign_id IS NOT NULL;

-- part.session_id repeats its message's session: a denormalized FK so
-- session-scoped part scans never join through message. The importer
-- keeps the two in agreement; nothing in DDL can check across rows.
CREATE TABLE part (
  id TEXT PRIMARY KEY,
  message_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  type TEXT NOT NULL DEFAULT '',
  tool_call_id TEXT NULL,
  signature TEXT NULL,
  data TEXT NOT NULL DEFAULT '{}',
  foreign_id TEXT NULL,
  FOREIGN KEY (message_id) REFERENCES message(id) ON DELETE CASCADE,
  FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE,
  UNIQUE (message_id, seq)
);

-- import_state has no id column: source is the key. It holds cursor
-- state no transcript on disk re-derives, which is why the migrations
-- table still stays out — cursors are rewritten, never migrated.
CREATE TABLE import_state (
  source TEXT PRIMARY KEY,
  cursor TEXT NOT NULL DEFAULT '',
  time_updated INTEGER NOT NULL
);

CREATE INDEX idx_session_worktree_updated ON session(worktree_id, time_updated);
CREATE INDEX idx_session_updated ON session(time_updated DESC);
-- The indexes above apply to fresh files only: the v1 gate checks tables,
-- never indexes, so a store created before an index landed keeps working
-- without it (correct answers, a slower plan) until krowk sessions
-- rebuild recreates the file from transcripts. There is no backfill path
-- in v1 — every row is re-derivable, so rebuild is the migration.
CREATE INDEX idx_turn_session ON turn(session_id);
CREATE INDEX idx_session_parent ON session(parent_id);
CREATE INDEX idx_binding_session ON session_binding(session_id);
-- Covering for the per-turn model lookup pricing does on every listing:
-- (turn_id, role, seq) finds a turn's last assistant message, and model and
-- provider are read from the index without touching the row.
CREATE INDEX idx_message_turn ON message(turn_id, role, seq, model, provider);
CREATE INDEX idx_part_message ON part(message_id);
CREATE INDEX idx_part_session ON part(session_id);
