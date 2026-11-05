Read SPEC.md before any work. Follow its milestones strictly.
Current milestone: M3 (code only; see PROGRESS.md for status).
README, PROGRESS.md and all files in docs/ are written in Russian.
Code comments in Russian. Identifiers, logs and commit messages
stay in English. Mark anything not confirmed by official docs as
UNVERIFIED.

Testing: Claude Code writes code and offline tests only (unit,
property, fixture, simulated venue). It never runs acceptance,
testnet or live tests and never places orders; the operator tests
everything on his own server (SPEC.md, "Testing responsibility").

Live trading is off by default: engine.mode defaults to "paper";
live needs mode = "live" in config AND the CLI flags
--live --i-understand-real-money. Every venue keeps
trading_enabled = false by default. No keys in the repository.

Workflow: one milestone = one branch = one pull request; each
branch is based on the previous milestone's branch. After each
milestone update "Current milestone" here and PROGRESS.md.
Before each PR: cargo fmt, clippy -D warnings, test, cargo-deny,
cargo-audit must pass.
