"""Zero-cost operations helpers that run next to the trading processes.

- `status_push`: ships `state/status.json` (a `StatusSnapshot`) to the dashboard's KV every few minutes.
- `kill_poller`: mirrors the dashboard Kill flag into `ops/KILL`, and provides the human-only `unkill`.
- `selfcheck`: daily disk / clock / expiry / lease / backup / quota check that files ACTION NEEDED items.
- `watchdog`: restarts unhealthy containers and enforces the post-lock kill.
- `costs`: `make costs`, the audit that every external service is still on its free tier.

None of these place orders, and none send alerts. Everything they learn ends up in the status snapshot,
which the dashboard shows and the daily summary Worker reports.
"""
