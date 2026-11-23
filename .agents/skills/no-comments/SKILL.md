---
name: no-comments
description: Codebase hygiene skill for open-ci based on "Your Codebase Is Memory". Strips stale, narrating, and workaround inline comments, converts claimed constraints into types/lints/tests, and bans copy-pasted footguns.
---

# No Comments & Footgun Removal ("Your Codebase Is Memory")

Agents pattern-match whatever already exists in the repository. Every sloppy workaround, stale comment, or half-baked shortcut becomes a template future runs copy across the codebase. Keep the code self-evident and enforce constraints mechanically instead of narrating them in comments.

## 1. Strip Low-Signal Inline Comments

Delete inline `//` comments that:
- **Narrate the obvious**: `// Increment counter`, `// Parse JSON`, `// Check if empty`.
- **Excuse a workaround or hack**: `// HACK: sleep 100ms so SQLite finishes`, `// TODO: validate later`, `// Temporary fallback`. Fix the root cause instead of apologizing in prose.
- **Duplicate a type or function name**: Rename the symbol or extract a well-named helper so the code reads plainly without commentary.
- **Drift from behavior**: Any comment describing how a function *used* to work or claiming an invariant that is not checked by a type, lint, or test.

## 2. Convert Claimed Invariants into Mechanical Walls

Whenever you delete a comment that states a real rule (e.g., `// Must never run with IAM attached` or `// Caller must sanitize region`), do **not** just lose the knowledge — push it up the Pyramid:
1. **Type / API design**: Encode it in a newtype, enum variant, or `Config::validate` check so invalid states cannot compile or load.
2. **Lint / Policy check**: Add a workspace Clippy lint or a static check in `scripts/verify.sh` with an error message explaining the root-cause fix.
3. **Executable test**: Add a `proptest` invariant in `crates/open-ci-core/tests/invariants.rs`, a real SQLite race test, or a loopback wire-contract test.

## 3. What Comments ARE Worth Keeping

Keep comments **rare and high-signal**:
- Concise `//!` module summaries and `///` public Rustdoc on library APIs (`open-ci-core`, `open-ci-provider`, `open-ci-store`, `open-ci-cache`) where callers genuinely benefit.
- Non-obvious protocol, crypto, or OS-level invariants that **cannot** be expressed in Rust's type system (e.g., why AWS EC2 Query API requires specific SigV4 canonical header ordering, or why `PRAGMA synchronous = FULL` is required with WAL). Keep them to 1–2 crisp lines.

## 4. Ban Footguns Before They Spread

During any feature or cleanup pass, hunt for patterns you would never want an agent to copy:
- Arbitrary `tokio::time::sleep(...)` in tests instead of deterministic `Clock` injection or readiness polling.
- Band-aid quick fixes (`unwrap_or_default()` hiding parse errors, swallowing errors with `let _ = ...` on I/O or DB writes).
- Hardcoded cloud regions, machine types, pool names, or vendor fallbacks in production code.
- Duplicated ad-hoc shell/curl loops where a shared deterministic script (`scripts/verify.sh` or `.agents/skills/verify-open-ci/scripts/drive.sh`) should exist.

## 5. Verification

After stripping comments and tightening types/helpers:
```bash
./scripts/verify.sh --fast
```
Confirm zero behavior regressions, zero clippy warnings, and cleaner diffs before running `ship-pr`.
