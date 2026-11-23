---
name: ship-pr
description: Clean shipping skill for open-ci. Leads PRs with empirical proof artifacts first (HTTP + SQLite + ./scripts/verify.sh), enforces no-comments hygiene, small Conventional Commits, zero AI slop, and concise Attention callouts.
---

# Ship PR (Proof Artifacts First, Clean Commits & Unslop)

Human reviewers should check **empirical proof artifacts first**, then skim the diff. Use this skill only after code is complete, `no-comments` hygiene is clean, and full `./scripts/verify.sh` exits `0` on the exact final tree.

## 1. Pre-Flight Checklist

1. Inspect `git status` and `git diff --stat`:
   - Zero untracked temp files, `.db` files, or `.audit/` logs.
   - Diff stays strictly scoped to the task's Goal + Check.
   - Run a quick `no-comments` pass on your diff: zero narrating/excuse inline comments or copy-pasted footguns.
2. Run the full canonical verification ladder on the exact final `HEAD`:
   ```bash
   ./scripts/verify.sh
   ```
   If you edit even one line afterward, re-run `./scripts/verify.sh`.

## 2. Small Conventional Commits

Keep history bisectable with Conventional Commits (`feat`, `fix`, `refactor`, `test`, `ci`, `chore`, `docs`):
- Subject line `<= 72` chars, imperative mood, optional crate/area scope: e.g. `fix(store): enforce atomic bootstrap consumption`.
- One logical unit per commit; never lump unrelated formatting or drive-by edits into feature commits.

## 3. Unslop Pass (Zero AI Writing Tells)

Strip every trace of filler from commit messages, PR body, comments, and docs:
- **Delete**: "delve", "comprehensive", "robust", "seamless", "leverage", "holistic", "elevate", "streamline", "testament to", "it's worth noting", "in summary", puffery, or line-by-line diff narration.
- **Keep**: Concrete nouns, exact paths/symbols, observed command outputs, HTTP + SQLite DB state transitions, and genuine trade-offs.
- Match the structure of `.github/PULL_REQUEST_TEMPLATE.md`.

## 4. Lead With Proof Artifacts First

Put empirical proof at the top of the PR description so reviewers verify behavior before reading lines of code:
- Exact summary of `./scripts/verify.sh` (all 7 gates passing on final `HEAD`).
- Action → HTTP/runtime state → SQLite DB side-effect output from `.agents/skills/verify-open-ci/scripts/drive.sh` (and any targeted `proptest` / wire-contract commands).
- Never claim a command passed unless you ran it on the exact final commit.

## 5. Concise `Attention` Callout Block

Surface every non-obvious risk up front so human review stays focused on what matters:

```markdown
## Attention

- **Trust boundary**: `crates/open-ci-github/src/oidc.rs` now rejects tokens missing `repository_owner_id`.
- **Migration / Compatibility**: None (or exact schema/config migration steps).
- **Not covered by local proof**: e.g. live AWS STS `AssumeRole` against a real AWS account (verified via loopback SigV4/XML contract test).
```

If there are zero subtle risks, write `- None — standard scoped change verified end-to-end by ./scripts/verify.sh.` instead of inventing fluff.
