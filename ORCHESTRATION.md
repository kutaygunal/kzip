# kzip Gap-Closure Orchestration

**Orchestrator:** main agent (w3:p1)
**Source of truth for the gap:** `results/verification-report.md`
**Existing high-level plan:** `PLAN.md`

## Roles

| Agent | Pane | Duty |
|-------|------|------|
| `planner` | w3:p2 | Divide the gap into phases, prioritize them, produce the phase plan. |
| `scrum-master` | w3:p3 | Review the plan, assign each phase to the engineer, write test cases for each phase. Does NOT implement. |
| `senior-engineer` | w3:p4 | Expert in zipping tools, Rust, and C. Implements each phase. |
| `testing` | w3:p5 | Tests each phase. On failure → back to engineer. On pass → DevOps. |
| `devops` | w3:p6 | Commits all changes and pushes to remote. |
| orchestrator (me) | w3:p1 | Coordinates the loop, closes non-essential agents between phases, tracks status. |

## Workflow loop (per phase)

1. **planner** produces/updates the phased plan (prioritized).
2. **scrum-master** reviews the plan, assigns the current phase to **senior-engineer**, and writes test cases for that phase.
3. **senior-engineer** implements the phase.
4. **senior-engineer** reports "done" to **testing**.
5. **testing** runs the phase tests.
   - FAIL → sends back to **senior-engineer** (loop to step 3).
   - PASS → hands off to **devops**.
6. **devops** commits all changes and pushes to remote.
7. **devops** reports "phase finished" to the orchestrator.
8. Orchestrator closes all subagents **except** `scrum-master` and `planner`.
9. Orchestrator informs `scrum-master` and `planner` the phase is done.
10. **scrum-master** starts the next loop → assigns next phase to engineer.
11. Loop until all phases are complete.

## Phase status tracker

Plan source: `results/phase-plan.md` (planner). Note: §5 items 1–6, 9 already CLOSED by
commits 29ddb0c/a2c4973/36dcc81/ee3b693 — do NOT re-assign. Loop starts at Phase 1.

| Phase | Description | Priority | Status | Assigned to | Test cases | Committed |
|-------|-------------|----------|--------|-------------|------------|-----------|
| 1 | Encryption: ZipCrypto (PKWARE) read+write | Medium | DONE | senior-engineer | results/phase1-tests.md | 27cdd73 |
| 2 | Encryption: WinZip AES read+write | Medium | DONE | senior-engineer | results/phase2-tests.md | c647936 |
| 3 | Write-path metadata: comments, extra fields, mtime, attrs, compression | Medium | IN_PROGRESS | senior-engineer | results/phase3-tests.md | |
| 4 | Streaming zip_source_* core (file/function/layered/window/zip) | Low-Med | IN_PROGRESS | senior-engineer-4 | results/phase4-tests.md | |
| 5 | zip_open_from_source / zip_fdopen + write-mode sources | Low-Med | PENDING | | | |
| 6 | Progress & cancel callbacks | Low-Med | PENDING | | | |
| 7 | Archive flags, unchange*, method-query, file-error APIs | Low-Med | PENDING | | | |
| 8 | Win32 sources + source utility helpers | Low-Med | PENDING | | | |

## Communication protocol

- All agents work in the same repo `C:/Users/kutay/Desktop/Projects/LibzipInRust`.
- The orchestrator drives the loop via `herdr agent prompt <name> "..." --wait`.
- Agents report status by writing to this file's tracker and/or replying to prompts.
- Do NOT modify `zip-core` core logic unless a phase explicitly requires it (per the
  verification report's constraint, the ZIP64 fix is the exception that requires it).
