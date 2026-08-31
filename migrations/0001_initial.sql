-- Refinery's durable case snapshots and immutable operation history.
-- JSON columns carry frozen versioned contracts; columns used for scheduling,
-- lookup, and consistency are normalized so SQLite can enforce their rules.
PRAGMA foreign_keys = ON;

CREATE TABLE cases (
  id TEXT PRIMARY KEY NOT NULL,
  idempotency_key TEXT NOT NULL UNIQUE,
  content_digest TEXT NOT NULL,
  state TEXT NOT NULL,
  request_id TEXT NOT NULL,
  source_json TEXT NOT NULL,
  destination_json TEXT NOT NULL,
  repository_id TEXT,
  metadata_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  completed_at TEXT,
  failure_reason TEXT
);

CREATE TABLE case_inputs (
  case_id TEXT PRIMARY KEY NOT NULL REFERENCES cases(id) ON DELETE CASCADE,
  request_json TEXT NOT NULL,
  transcript_json TEXT NOT NULL,
  task_hint TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE attachments (
  id TEXT PRIMARY KEY NOT NULL,
  case_id TEXT NOT NULL REFERENCES cases(id) ON DELETE CASCADE,
  metadata_json TEXT NOT NULL,
  state TEXT NOT NULL,
  media_path TEXT,
  digest_sha256 TEXT,
  provider_file_name TEXT,
  uploaded_at TEXT,
  expires_at TEXT,
  rejected_reason TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX attachments_case_id_idx ON attachments(case_id);

CREATE TABLE case_events (
  id TEXT PRIMARY KEY NOT NULL,
  case_id TEXT NOT NULL REFERENCES cases(id) ON DELETE CASCADE,
  sequence INTEGER NOT NULL,
  occurred_at TEXT NOT NULL,
  type TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  UNIQUE(case_id, sequence)
);
CREATE INDEX case_events_case_sequence_idx ON case_events(case_id, sequence);

CREATE TABLE backend_runs (
  id TEXT PRIMARY KEY NOT NULL,
  case_id TEXT NOT NULL REFERENCES cases(id) ON DELETE CASCADE,
  backend_id TEXT NOT NULL,
  status TEXT NOT NULL,
  step INTEGER NOT NULL DEFAULT 0,
  history_json TEXT NOT NULL,
  usage_json TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  finished_at TEXT,
  error TEXT
);
CREATE INDEX backend_runs_case_id_idx ON backend_runs(case_id);

CREATE TABLE question_requests (
  id TEXT PRIMARY KEY NOT NULL,
  case_id TEXT NOT NULL REFERENCES cases(id) ON DELETE CASCADE,
  status TEXT NOT NULL,
  request_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  answered_at TEXT
);
CREATE UNIQUE INDEX question_requests_one_pending_per_case
  ON question_requests(case_id) WHERE status = 'pending';

CREATE TABLE answers (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  question_request_id TEXT NOT NULL REFERENCES question_requests(id) ON DELETE CASCADE,
  case_id TEXT NOT NULL REFERENCES cases(id) ON DELETE CASCADE,
  answer_json TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX answers_question_request_idx ON answers(question_request_id);

CREATE TABLE outputs (
  id TEXT PRIMARY KEY NOT NULL,
  case_id TEXT NOT NULL REFERENCES cases(id) ON DELETE CASCADE,
  prompt_json TEXT NOT NULL,
  valid INTEGER NOT NULL,
  validation_issues_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  accepted_at TEXT
);
CREATE INDEX outputs_case_id_idx ON outputs(case_id);

CREATE TABLE deliveries (
  id TEXT PRIMARY KEY NOT NULL,
  case_id TEXT NOT NULL REFERENCES cases(id) ON DELETE CASCADE,
  output_id TEXT REFERENCES outputs(id),
  destination_kind TEXT NOT NULL,
  attempt INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  receipt_json TEXT,
  retry_class TEXT,
  error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(id, attempt)
);
CREATE INDEX deliveries_case_id_idx ON deliveries(case_id);

CREATE TABLE repositories (
  id TEXT PRIMARY KEY NOT NULL,
  canonical_path TEXT NOT NULL UNIQUE,
  policy_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE jobs (
  id TEXT PRIMARY KEY NOT NULL,
  case_id TEXT REFERENCES cases(id) ON DELETE CASCADE,
  operation_id TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  status TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  max_attempts INTEGER NOT NULL,
  run_at TEXT NOT NULL,
  lease_owner TEXT,
  lease_expires_at TEXT,
  last_heartbeat_at TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  completed_at TEXT
);
CREATE INDEX jobs_claim_idx ON jobs(status, run_at, lease_expires_at);
CREATE INDEX jobs_case_lease_idx ON jobs(case_id, status, lease_expires_at);

CREATE TABLE settings (
  key TEXT PRIMARY KEY NOT NULL,
  value_json TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
