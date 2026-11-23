# Contributing to open-ci

Thank you for contributing to `open-ci`! Whether you are a human contributor or an AI coding agent, this repository uses the same mechanical engineering standards and verification gates.

Start here:
- [**AGENTS.md**](AGENTS.md) — Core repository rules and agent workflow entry point.
- [**Engineering Principles & Trust Loop**](docs/engineering.md) — The Pyramid of Trust, `.agents/skills/`, reusable runtime + SQLite driver, and the 7-gate proof ladder.
- [**Architecture Guide**](docs/architecture.md) — Crate boundaries, concurrency model, and exact HTTP API routes.
- [**Security & Threat Model**](docs/security.md) — Trust boundaries and non-negotiable security invariants.

---

## Engineering Non-Negotiables

1. **Strict Hexagonal Boundaries:** Keep `crates/open-ci-core` 100% pure (zero network, filesystem, or async runtime dependencies). All side effects belong in `open-ci-store`, `open-ci-github`, `open-ci-provider-*`, `open-ci-server`, `open-ci-cache`, `open-ci-aws-janitor`, or `open-ci-cli`.
2. **Workspace Lints Stay Strict:** `Cargo.toml` enforces `unsafe_code = "forbid"`, `clippy::unwrap_used = "deny"`, `clippy::expect_used = "deny"`, `clippy::panic = "deny"`, `clippy::todo = "deny"`, `clippy::unimplemented = "deny"`, and `clippy::pedantic = "warn"`. Keep modules small and focused; never add `#[allow(...)]` to silence Clippy.
3. **Provider & Workload Neutrality:** Never hardcode cloud regions, zones, machine types, pool names, or third-party runner vendors into production code or defaults. Everything cloud- or workload-specific must be driven by configuration.
4. **Your Codebase Is Memory (`no-comments`):** Fix root causes rather than band-aid quick fixes, ban copy-pasted footguns, and strip narrating or workaround inline comments — encode invariants in types, lints, or tests instead.
5. **Real Verification (Zero `#[ignore]`):**
   - Add a `proptest` property test in `crates/open-ci-core/tests/invariants.rs` for every new domain invariant.
   - Add loopback HTTP contract tests for every cloud adapter, GitHub client, cache backend, or HTTP handler change.
   - Keep `docs/architecture.md`, `.agents/skills/verify-open-ci/features.md`, and `.agents/skills/verify-open-ci/scripts/drive.sh` in parity whenever adding or modifying HTTP routes or CLI commands.

---

## Local Verification Before Opening a PR

Run the unified 7-gate proof runner:

```bash
./scripts/verify.sh
```

Or run individual checks directly:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run --bin open-ci -- validate --config config/open-ci.example.toml
./.agents/skills/verify-open-ci/scripts/drive.sh
node --test actions/route/test/route.test.ts
terraform fmt -check -recursive infra/terraform
shellcheck infra/packer/scripts/install-runner.sh infra/packer/scripts/setup-nvme.sh
```

---

## Pull Request Workflow

1. **Goal + Check first:** Define the goal and failing repro check before editing; isolate non-trivial work in a clean branch or `git worktree`.
2. **Prove it empirically:** Run `./scripts/verify.sh` and confirm all 7 gates (including live HTTP + SQLite DB verification via `drive.sh`) pass with zero skipped or ignored tests.
3. **Garden the diff (`no-comments`):** Strip narrating/workaround inline comments and confirm no band-aid shortcuts or security regressions against [`docs/security.md`](docs/security.md).
4. **Encode corrections:** If a recurring bug or edge case surfaces, push the guard up the Pyramid (type constraint, lint, `proptest`, wire contract, or `drive.sh` check) so it cannot regress.
5. **Lead with proof artifacts:** Open the PR with observed `./scripts/verify.sh` output first, followed by a concise summary and `Attention` callouts.

## Security Disclosures

Do **not** open a public issue or PR for security vulnerabilities. Follow the private reporting process in [SECURITY.md](SECURITY.md).
