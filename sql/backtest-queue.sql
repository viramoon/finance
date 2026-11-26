CREATE TABLE IF NOT EXISTS jobs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  idem TEXT UNIQUE NOT NULL,
  request TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'queued'
    CHECK(status IN ('queued','running','succeeded','failed','timed_out')),
  attempt INTEGER NOT NULL DEFAULT 0 CHECK(attempt BETWEEN 0 AND 2),
  summary TEXT,
  error TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS jobs_status_id ON jobs(status,id);

CREATE TABLE IF NOT EXISTS strategy_backtest_versions (
  id TEXT NOT NULL,
  version TEXT NOT NULL,
  content_sha256 TEXT NOT NULL,
  format TEXT NOT NULL CHECK(format IN ('yaml','json')),
  source BLOB NOT NULL,
  inputs TEXT NOT NULL,
  PRIMARY KEY (id,version)
) STRICT;

CREATE TABLE IF NOT EXISTS strategy_backtest_snapshots (
  snapshot_sha256 TEXT PRIMARY KEY NOT NULL,
  content BLOB NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS job_leases (
  job_id INTEGER PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE,
  expires_at_ms INTEGER NOT NULL
);
