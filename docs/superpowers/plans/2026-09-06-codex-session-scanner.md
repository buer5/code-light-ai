# Codex Session Scanner Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reliably derive Code Light's Codex tray state from current Codex session JSONL logs, while retaining hook files as a fallback.

**Architecture:** A focused `codex_sessions` Rust module will discover a bounded number of recent rollout files below `~/.codex/sessions`, parse only their JSONL records, and reduce each session's task events into a normalized state. `lib.rs` will merge scanner and hook sessions by ID before the existing global state-priority calculation. Log results are authoritative for overlapping Codex sessions; Claude handling and tray rendering are unchanged.

**Tech Stack:** Rust 2021, Tauri v2, `serde_json`, standard-library filesystem and time APIs.

---

## File map

- Create: `src-tauri/src/codex_sessions.rs` — bounded Codex JSONL discovery, event reduction, and unit tests.
- Modify: `src-tauri/src/lib.rs` — include the scanner and merge normalized sessions into the existing polling path.
- Modify: `README.md` and `README_CN.md` — document log-first Codex status with hook fallback.

### Task 1: Reduce Codex JSONL events into testable session state

**Files:**
- Create: `src-tauri/src/codex_sessions.rs`

- [ ] **Step 1: Write failing scanner tests**

Create temporary `sessions/2026/09/06/*.jsonl` fixtures whose `session_meta.payload.id` identifies a session. Add tests that expect:

```rust
assert_eq!(sessions[0].state, CodexSessionState::Working);
```

after a current `event_msg.payload.type = "task_started"`, and:

```rust
assert_eq!(sessions[0].state, CodexSessionState::Completed);
```

after a later `task_complete` or `turn_aborted` for the same `turn_id`.

- [ ] **Step 2: Run the focused test and verify RED**

Run: `cargo test codex_sessions --lib`

Expected: compilation failure because `codex_sessions` does not yet exist.

- [ ] **Step 3: Implement the smallest scanner**

Add `CodexSession { session_id, state, message, timestamp }` and `scan_codex_sessions(root, now)`. Recursively collect only `*.jsonl` files, sort by modification time, limit to 16, reject logs older than five minutes, and ignore malformed JSON. Track active turn IDs: add on `task_started`, remove on `task_complete` or `turn_aborted`; return `Working` if any turn remains active, `Completed` after a terminal event, otherwise `Idle`.

- [ ] **Step 4: Run scanner tests and verify GREEN**

Run: `cargo test codex_sessions --lib`

Expected: all new scanner tests pass.

- [ ] **Step 5: Commit the scanner unit**

```bash
git add src-tauri/src/codex_sessions.rs
git commit -m "feat: scan Codex session activity"
```

### Task 2: Merge scanner results with existing Code Light sessions

**Files:**
- Modify: `src-tauri/src/lib.rs:55-203`
- Test: `src-tauri/src/lib.rs` test module

- [ ] **Step 1: Write failing merge tests**

Add tests for a pure merge helper showing that a scanned `working` Codex session replaces a hook-backed `completed` session with the same ID, while a hook-only Claude session remains present.

```rust
assert_eq!(merged["codex-1"].state, State::Working);
assert_eq!(merged["claude-1"].state, State::Waiting);
```

- [ ] **Step 2: Run focused test and verify RED**

Run: `cargo test merge_scanned_codex --lib`

Expected: failure because the merge helper is not defined.

- [ ] **Step 3: Implement the minimal merge and polling integration**

Refactor the local read representation to retain `session_id` and source. Read hook data as today, call `codex_sessions::scan_codex_sessions(~/.codex/sessions, now)`, and insert scanner records under `codex:<session-id>`. Replace only matching Codex hook records, then feed all records through the current stale/waiting/completed and priority rules. A scanner error must be ignored so the hook fallback continues to work.

- [ ] **Step 4: Run focused merge test and verify GREEN**

Run: `cargo test merge_scanned_codex --lib`

Expected: the Codex status precedence and Claude preservation assertions pass.

- [ ] **Step 5: Run all Rust unit tests**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`

Expected: all tests pass.

- [ ] **Step 6: Commit integration**

```bash
git add src-tauri/src/lib.rs src-tauri/src/codex_sessions.rs
git commit -m "feat: merge Codex log status into tray state"
```

### Task 3: Synchronize user-facing behavior documentation

**Files:**
- Modify: `README.md:49-54,171-180`
- Modify: `README_CN.md:49-54,171-180`

- [ ] **Step 1: Update the status architecture descriptions**

State that Codex status uses session-log scanning as the primary source and hooks as compatibility fallback; Claude remains hook-driven. Do not claim support for providers outside Claude and Codex.

- [ ] **Step 2: Verify the documentation and working tree**

Run: `git diff --check && git status --short`

Expected: no whitespace errors and only the intended documentation files are unstaged.

- [ ] **Step 3: Commit documentation**

```bash
git add README.md README_CN.md
git commit -m "docs: explain Codex log status tracking"
```

### Final verification

- [ ] Run: `cargo fmt --manifest-path src-tauri/Cargo.toml --check`
- [ ] Run: `cargo test --manifest-path src-tauri/Cargo.toml`
- [ ] Run: `git diff --check`
- [ ] Confirm each implementation commit is present and the working tree is clean except for user-owned changes.
