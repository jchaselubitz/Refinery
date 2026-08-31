# Migrations

SQLx migrations for the SQLite database in the data directory. The initial
schema — `cases`, `case_inputs`, `attachments`, `case_events`, `backend_runs`,
`question_requests`, `answers`, `outputs`, `deliveries`, `repositories`, `jobs`,
and `settings` — is established by `0001_initial.sql` in milestone M2.

Migrations are embedded in the binary and applied on startup, so a released
build never needs the SQLx CLI to open its own database.
