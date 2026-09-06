# Codex Session Scanner Design

## Goal

Make Code Light determine Codex activity from the current Codex session-log format instead of relying solely on lifecycle hooks. Existing Claude tracking and tray presentation remain unchanged.

## Decision

Add a small, self-contained Rust scanner to the Tauri backend. It reads the most-recent files under `~/.codex/sessions/**` and `~/.codex/history.jsonl`, derives status from Codex event records, and merges the result with the existing hook-backed session files.

Codex log data is authoritative when the same session is reported by both sources. Hook data remains a fallback for installations or Codex versions that do not expose usable log records.

## State mapping

| Codex evidence | Code Light state |
| --- | --- |
| Active `task_started` without a later completion/abort event | `working` |
| Permission-request hook data | `waiting` |
| Latest `task_complete` or `turn_aborted` | `completed` |
| No recent Codex session | `idle` |

The existing global priority remains `error > waiting > working > completed > idle`.

## Components and data flow

1. `CodexSessionScanner` discovers a bounded set of recent `*.jsonl` files below `~/.codex/sessions`.
2. It reads event records, tracks activity by `turn_id`, and emits a normalized session snapshot containing session ID, state, message, timestamp, and source.
3. `read_all_sessions` reads hook-backed files as it does today, then merges them with scanner snapshots by session ID.
4. The existing tray update loop consumes the merged state unchanged.

Only recent files and their bounded head/tail content are parsed per polling cycle. File-metadata caching is used where practical so an unchanged log is not reprocessed.

## Compatibility and failure behavior

- Missing, unreadable, or malformed Codex files are ignored; they must not prevent hook or Claude status updates.
- The scanner must not write to `~/.codex`.
- A stale session is excluded using the existing five-minute window.
- Current hook setup remains in place to avoid changing an installed user's configuration or trust requirements.

## Testing

Unit tests use temporary `.codex/sessions` fixtures and verify:

1. `task_started` is reported as working.
2. `task_complete` and `turn_aborted` are reported as completed.
3. A complete event overrides earlier activity from the same turn.
4. Malformed input and old session files are safely ignored.
5. Log-derived Codex status overrides the matching hook-backed session, while a non-overlapping hook session remains available.

## Scope boundaries

This migration does not import EchoIsland crates, add new agent providers, change the UI, or replace the existing Claude hook integration.
