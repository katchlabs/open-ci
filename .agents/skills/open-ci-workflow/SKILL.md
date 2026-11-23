---
name: open-ci-workflow
description: Front-door engineering loop for open-ci tasks (feature, bug-fix, refactor, autonomous-run). Enforces Goal + Check without planning theater, root-cause fixes over band-aids, stop-the-line gates, and worktree isolation.
---

# `open-ci` Engineering Loop

Invest tokens in empirical verification and clean architecture, not ceremony. Start every task with a **Goal + Check** ("repro first, then fix and verify"), fix root causes rather than band-aids, and halt immediately when any gate fails.

## 1. Classify the Track

| Track | When to use | Mandatory loop |
| --- | --- | --- |
| `feature` | New capability, route, provider feature, CLI subcommand, or cache behavior | `open-ci-workflow` → `prove-invariants` → `verify-open-ci` → `no-comments` → `ship-pr` |
| `bug-fix` | Regression, race, security defect, or broken contract | `open-ci-workflow` → failing repro first → root-cause fix → `prove-invariants` / `verify-open-ci` → `encode-correction` → `ship-pr` |
| `refactor` | Structural cleanup with zero intended behavior change | `open-ci-workflow` → baseline `./scripts/verify.sh` → small verifiable commits → `no-comments` → `./scripts/verify.sh` → `ship-pr` |
| `autonomous-run` | Multi-step or unattended execution loop | Prove inner loop locally first → isolated `git worktree` → `.audit/<slug>.tsv` log → unit-by-unit proof → `ship-pr` |

Skip adversarial agent diff-arguing loops — spend those tokens running the real compiled binary (`verify-open-ci`), property tests (`proptest`), real SQLite concurrency races, and loopback wire contracts (`prove-invariants`).

## 2. Goal + Check (No Planning Theater)

The best spec is code + a failing check. Before editing, state a crisp **Goal + Check** instead of writing multi-page planning prose:

```text
GOAL: Reject replayed worker bootstrap tokens atomically under concurrency.
CHECK:
1. Command(s) that MUST exit 0: `./scripts/verify.sh` + `cargo test -p open-ci-store atomic_bootstrap`
2. Observable runtime + DB state: `POST /v1/workers/{name}/bootstrap` returns 200 once, 409 on replay, and SQLite sets `bootstrap_used = 1`.
3. Non-negotiables: zero `#[allow(`, `open-ci-core` zero I/O, no lock across `.await`.
```

- **Bug fixes**: Write the failing repro test or driver check **first**, watch it fail, then implement the root-cause fix.
- **Never weaken a check** to declare victory. If requirements change, update the check explicitly and record why.

## 3. Root Causes Only ("Your Codebase Is Memory")

Agents replicate whatever patterns exist in the repo. Never ship the "smallest quick band-aid fix" (arbitrary sleeps, swallowed errors, duplicate curl hacks, inline workaround comments). Fix the underlying abstraction or data structure, and run `no-comments` to strip stale or narrating comments before shipping.

## 4. Inner Loop First, Outer Loop Second

- Prove the single-agent loop works reliably on one task before running parallel tracks or autonomous loops.
- For parallel tracks, risky refactors, or autonomous runs, isolate from trunk with `git worktree`:
  ```bash
  git worktree add "../open-ci-<slug>" -b "<type>/<slug>" main
  ```
- Remove merged or abandoned worktrees with `git worktree remove "../open-ci-<slug>"`.

## 5. Verifiable Units & Stop the Line

Sequence changes along the workspace boundaries:
1. Pure domain types & validation in `crates/open-ci-core` (+ `proptest` invariants).
2. Durable SQLite schema/queries in `crates/open-ci-store` (real SQLite tests).
3. Wire adapters (`open-ci-provider-*`, `open-ci-github`, `open-ci-cache`) with loopback Axum contract tests.
4. Control plane / CLI wiring (`open-ci-server`, `open-ci-cli`, `actions/route`) + real-binary driver (`verify-open-ci`).
5. IaC & documentation parity (`infra/`, `docs/architecture.md`, `.agents/skills/verify-open-ci/features.md`).

**Stop the Line**: Any failing gate halts the pipeline immediately. Never forward broken code to a later step. Run `./scripts/verify.sh --fast` after each unit and full `./scripts/verify.sh` before finishing.

## 6. Multi-Step Audit Log (`.audit/<slug>.tsv`)

For multi-step or `autonomous-run` tasks, append one line per milestone to `.audit/<slug>.tsv` (local scratch only — **never commit `.audit/`**):

```tsv
ts	phase	decision	why	evidence	result
2026-11-23T17:00:00Z	check	repro-first	confirm bug reproduces before fix	cargo test -p open-ci-store	FAIL
2026-11-23T17:12:00Z	verify	full-ladder	prove root-cause fix + zero regressions	./scripts/verify.sh	PASS
```

## 7. Escalation Rules

Stop and ask the human before:
- Changing public HTTP routes or schemas, adding workspace dependencies, modifying IAM/OIDC/HMAC trust boundaries, or editing SQLite migrations.
- Repeating a third guess after two consecutive failed attempts — diagnose the root cause and lock the prevention in via `encode-correction`.
