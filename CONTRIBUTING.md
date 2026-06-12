# Contributing to swindex

Thanks for considering a contribution. This project follows a strict trunk-based workflow with a few non-negotiable rules. Reading this whole file before opening a PR will save you a round trip.

## 1. Issue first, PR closes

Every PR is backed by a GitHub issue that defines the work. The discipline:

1. **Open an issue** with a clear title, scope, and acceptance criteria. Label it (`type:algorithm`, `type:infrastructure`, `type:docs`, `type:site`, optionally `layer:0..3`). Assign to a milestone (`v0.1`, `v0.2`).
2. **Discuss the approach** in the issue if it's non-trivial. Don't sink time into a PR for an architectural change without confirming the direction.
3. **Branch from `main`** with a short name describing the change: `feat/short-name`, `fix/short-name`, `docs/short-name`. One branch per issue.
4. **PR body** opens with `Closes #N` so the issue auto-closes when the PR merges.
5. **Merge** uses squash strategy. The branch is auto-deleted.

## 2. Trunk-based discipline

- **`main` is always releasable.** Every commit on `main` passes the three Mandate-5 gates (below). Branch protection enforces this — direct pushes to `main` are blocked.
- **Sequential, not parallel.** One open PR at a time when working solo. (Multiple people: one open PR per person.) Long-lived branches are the enemy of small reviewable diffs.
- **Branches are short-lived.** Open → push → review → merge → delete. Aim for hours, not days.
- **No force-push to `main`** (blocked by branch protection). Force-push to feature branches during review is allowed but please mention it.

## 3. Mandate-5 gates

Every PR must pass these three checks. CI enforces them; you should also run them locally before pushing.

```bash
cargo fmt --all -- --check                                   # rustfmt clean
cargo clippy --all-targets --all-features -- -D warnings     # clippy clean, warnings = errors
cargo test --workspace --all-features                         # all tests pass
```

The clippy bar in particular is high: `clippy::all = deny`, `clippy::pedantic = warn`, escalated to errors by `-D warnings`. Read the suggestions; they're usually right. If you genuinely need to suppress one, use `#[allow(clippy::specific_lint)]` with a comment explaining why — not a crate-wide allow.

## 4. PR body template

Use this shape (the project's existing PRs are good examples):

```markdown
Closes #N.

## Summary
One-paragraph description of what changes and why.

## What's in
- file/module 1 — brief
- file/module 2 — brief

## Acceptance criteria (from #N)
- [x] ...
- [x] ...

## Tests added
- `test_name` — what it protects against
- ...

## Verification (Mandate 5)
```
$ cargo fmt --all -- --check                                # clean
$ cargo clippy --all-targets --all-features -- -D warnings  # clean
$ cargo test --workspace --all-features
test result: ok. N passed; 0 failed
```

## Notes for review
- The hard parts to second-guess
- Anything I'm not sure about
```

## 5. Code conventions

- **`#![allow(unsafe_code)]` is forbidden.** `unsafe_code = "forbid"` at the crate root. If you genuinely need unsafe, open an issue first.
- **Doc comments document _why_, not _what_.** Names should communicate _what_; comments should explain rationale, invariants, footguns, and references to research papers / design doc sections.
- **Tests document intent.** Every test should have a `///` doc comment saying what it protects against. Not "tests function X" — what regression would land if you deleted it.
- **Deterministic by default.** `BTreeMap` over `HashMap` for any iteration-order-sensitive code. Seeded RNG; document the seed contract.
- **Errors implement `std::error::Error`.** Use `From<…>` for `?` propagation. No `Box<dyn Error>` mush in public APIs.
- **Format-stable on disk.** Any new persisted value gets a format-version byte. Decoders refuse mismatched versions.

## 6. Algorithm changes

Anything that touches `src/community.rs`, `src/graph.rs`, `src/hub*.rs`, or `src/region.rs` is mathematically load-bearing. Required for the PR:

- A hand-computed invariant test if the math is new (e.g., `aggregation_preserves_modularity_exactly` is the pattern).
- A real-graph test on Zachary or an LFR fixture demonstrating the algorithm's published-result range.
- A determinism test (same seed → same partition).

Don't change the modularity formula, the aggregation 2× doubling, or the self-loop convention without a `DESIGN.md` update explaining the rationale.

## 7. Persistence changes

Anything that touches `src/index.rs` storage layout requires:

- A round-trip test (`build → close → reopen → same answers`).
- A format-version bump if encoding changes.
- A migration note in `CHANGELOG.md`.

## 8. Documentation

- **`DESIGN.md` is the north-star, rustdoc is the ground truth.** `DESIGN.md` describes the *target* architecture and opens with an implementation-status table mapping each design element to what's actually shipped. For shipped behavior, the rustdoc (`src/lib.rs`, module docs) is authoritative. When the code diverges from the design in a way the status table doesn't capture, update the table in the same PR — don't let silent drift accumulate.
- **`BENCHMARKS.md` reflects reality.** If you re-run benchmarks, update the numbers in the file. Don't put claims in PR descriptions that aren't in the doc.
- **`CHANGELOG.md` updates per PR.** Add an entry under `[Unreleased]` describing the user-visible change.

## 9. Issues and milestones

- **`v0.1` milestone:** the working-index goal (closed; v0.1.0 released 2026-05-31).
- **`v0.2` milestone:** production-hardening (open). Brandes' betweenness, Ada-IVF incremental maintenance, benchmark expansion, time-travel, adversarial fixtures.
- **Labels:** `type:*` describes the work category; `layer:N-…` tags algorithm work to its index layer; `status:done` marks issues that already shipped.

## 10. License and CLA

By contributing, you agree your contribution is under the [BSL 1.1](LICENSE) license terms that swindex uses. There is no separate CLA. If you have employer constraints, sort that out before opening a PR.

## 11. Questions?

- File an issue on GitHub — this is the reliable channel. swindex is currently solo-maintained; expect a response from the maintainer, not a team.
- There is no Discord or project email yet (a channel is tracked in issue #32; this section will be updated when one exists).

Happy hacking.
