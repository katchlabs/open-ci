## Proof Artifacts First (`./scripts/verify.sh`)

<!-- Reviewers check empirical proof first, then skim the diff. Paste the real, unedited summary from running `./scripts/verify.sh` (including HTTP + SQLite DB side-effect checks from `drive.sh`) on the exact final HEAD. -->

```text
$ ./scripts/verify.sh
```

## Goal & Check

<!-- State what changed in 1-3 sentences and the concrete Goal + Check defined before editing (repro first -> root-cause fix -> verify). -->

- **Goal**:
- **Check**:
  - [ ] `./scripts/verify.sh` exits `0` on the final commit (all 7 gates passing).
  - [ ] Observable runtime / wire / SQLite DB side effect verified:
  - [ ] `no-comments` pass clean (no narrating/excuse comments, no band-aid footguns, no `#[allow(`).

## Attention

<!-- Surface anything deserving focused human review: trust boundaries, auth/crypto/OIDC, SQLite migrations, concurrency/locks, IaC IAM/network changes, or anything NOT covered by local proof. If none, state so explicitly. -->

- **Trust boundaries / Security / Concurrency**:
- **Migrations / Breaking changes**:
- **Not covered by local proof**:
