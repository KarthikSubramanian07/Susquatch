"""Synthetic election nights, needle priors, replays and drills.

* `history`: historical county results (MEDSL-style CSV) by vote type.
* `conventions`: per-state reporting order and timing assumptions.
* `timeline`: final results → realistic cumulative `CountyResultsBatch` stream (JSONL).
* `prior`: historical results + race prior margins → `NeedlePrior`.
* `needle_ref`, `backends`: the Python reference estimator and the optional C++ needle.
* `runner`: stream a replay to NATS or in process, with chaos (drop / malformed / kill).
* `drill`: executor simulation and `reports/drills/<date>.md`.
* `spec`, `download`, `closing`: inputs.

CLI: `uv run python -m susq.replay --help`. See docs/ELECTION_NIGHT.md.
"""
