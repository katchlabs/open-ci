---
name: encode-correction
description: Compounding quality loop for open-ci based on the Pyramid of Trust. Fixes recurring mistakes at the highest deterministic layer (Codebase -> Static Analysis -> Verification -> Prose), proven red-before-green.
---

# Encode Correction (Fix at the Highest Layer of the Pyramid)

When a bug, edge case, or agent mistake happens once, fix it cleanly. When a class of mistake could recur, **do not rely on human review or another paragraph in prose** — push the prevention up to the highest deterministic layer so the codebase itself enforces the rule at zero ongoing token cost.

## 1. The Correction Order (Pyramid of Trust)

Always start at Level 1 and only move down when the higher level genuinely cannot express the invariant:

| Priority | Layer | Where in `open-ci` | Why & Example |
| --- | --- | --- | --- |
| **1 (Best)** | **Codebase (API & Data Structures)** | `crates/open-ci-core`, newtypes, enums, `Config::validate`, module visibility | Make invalid states unrepresentable and ban footguns so agents cannot copy bad patterns (e.g., `SecretString` redacts `Debug`; `GitHubScope` enum enforces repo vs org scoping). |
| **2** | **Static Analysis (Lints, Types, CI)** | `[workspace.lints]` in `Cargo.toml`, Clippy `-D warnings`, `scripts/verify.sh` Gates 1/6/7 | Deterministic, instant, zero ongoing tokens. **Write failure messages that tell the agent how to fix the root cause** (e.g., banning `#[allow(`, enforcing `open-ci-core` dep purity, enforcing IMDSv2 `http_tokens = "required"`). |
| **3** | **Verification (Empirical, Property, Formal)** | `proptest` (`open-ci-core`), SQLite races (`open-ci-store`), Axum wire contracts, `.agents/skills/verify-open-ci/scripts/drive.sh` | Catch runtime, persistence, and protocol regressions automatically with real artifacts and DB assertions. |
| **4** | **Prose (`AGENTS.md`, skills)** | `AGENTS.md`, `.agents/skills/*/SKILL.md` | Last resort — keep minimal and only for judgment calls that cannot be checked mechanically. |
| **5 (Never rely on this)** | **Human Review** | PR review | Humans verify proof artifacts and design direction. Relying on humans to repeatedly catch the same mistake does not scale. |

## 2. Red-Before-Green Proof (Mandatory)

A guard that has never been observed failing proves nothing:

1. **Red**: On the unfixed code (or a temporary intentional violation), run the new compile check, lint, property test, or `verify.sh` gate and observe it **FAIL** with a clear diagnostic that explains the root-cause fix.
2. **Fix**: Apply the root-cause fix (never a band-aid workaround or `#[allow(...)]`).
3. **Green**: Re-run the new guard and full `./scripts/verify.sh`; observe both exit `0`.

## 3. Keep Deterministic Walls Sharp

- Static & policy checks in `scripts/verify.sh` MUST stay POSIX/bash-safe (macOS + Linux, zero Python deps) and print the exact offending file/symbol plus how to fix it.
- Mirror CI-critical gates in `.github/workflows/ci.yml` so local `./scripts/verify.sh` and GitHub Actions stay in lockstep.
- Never introduce timing-dependent sleeps; use injected `Clock` or bounded readiness polling.

## 4. Record the Compounding Win

In your commit message or PR description, state in 3 crisp lines:
- **Mistake prevented**: 1 sentence.
- **Pyramid layer chosen**: Codebase / Static Analysis / Verification.
- **Red → Green evidence**: exact command that failed before and passed after.
