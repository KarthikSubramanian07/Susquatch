"""Bridge configuration: ``config/susq.toml`` (non-secret defaults) + ``.env`` + the environment.

Precedence: process environment > ``.env`` > ``config/susq.toml``. Only these variables are read:
``SUSQ_HOME``, ``SUSQ_CONFIG``, ``SUSQ_API_KEY``, ``SUSQ_BASE_URL``, ``SUSQ_TOURNAMENT_SLUG``,
``SUSQ_MODE``. Relative paths in the TOML resolve against ``SUSQ_HOME`` (default: the working
directory, i.e. the repo checkout on the server).
"""

from __future__ import annotations

import os
import tomllib
from collections.abc import Mapping
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any

from dotenv import dotenv_values

__all__ = [
    "BridgeConfig",
    "CalendarConfig",
    "FairConfig",
    "OmsConfig",
    "PlatformConfig",
    "RacerConfig",
    "RiskConfig",
    "SniperConfig",
    "load_config",
]


def _utc(value: str) -> datetime:
    ts = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if ts.tzinfo is None:
        raise ValueError(f"calendar timestamp must carry a timezone: {value!r}")
    return ts


@dataclass(frozen=True)
class PlatformConfig:
    base_url: str
    alt_base_url: str
    tournament_slug: str
    request_timeout_ms: int
    initial_rps: float
    min_rps: float
    max_rps: float
    order_budget_frac: float


@dataclass(frozen=True)
class CalendarConfig:
    open_utc: datetime
    lock_utc: datetime
    last_order_utc: datetime


@dataclass(frozen=True)
class RiskConfig:
    per_market_gross_frac: float
    total_gross_frac: float
    glitch_through_fair_pts: float
    glitch_max_frac: float
    loss_per_hour_frac: float
    reject_breaker_per_min: int
    max_order_notional_frac: float
    clusters: dict[str, float] = field(default_factory=dict)
    #: Size buying power on modeled ALL effective cost instead of full notional (default off).
    use_collateral_buying_power: bool = False
    #: ``use_collateral_buying_power = "auto"``: off until measure_day1 confirms the engine matches the
    #: model, then on (the confirmation persists in state/bridge/collateral_confirmed.json).
    collateral_auto: bool = False


@dataclass(frozen=True)
class OmsConfig:
    default_ttl_s: float
    reconcile_interval_s: float
    idempotency_namespace: str


@dataclass(frozen=True)
class FairConfig:
    stale_after_s: float
    mapping_error_pts: float


@dataclass(frozen=True)
class RacerConfig:
    enabled: bool
    poll_interval_s: float
    min_edge: float
    max_bundle_notional_frac: float
    order_ttl_s: float
    refire_cooldown_s: float = 30.0
    max_relationship_notional_frac: float = 0.03


@dataclass(frozen=True)
class SniperConfig:
    enabled: bool
    min_edge_pts: float
    max_mistake_frac: float
    order_ttl_s: float


@dataclass(frozen=True)
class BridgeConfig:
    home: Path
    platform: PlatformConfig
    calendar: CalendarConfig
    risk: RiskConfig
    oms: OmsConfig
    fair: FairConfig
    racer: RacerConfig
    sniper: SniperConfig
    mode_default: str
    lease_path: Path
    lease_ttl_s: float
    lease_renew_s: float
    kill_file: Path
    go_live_file: Path
    journal_dir: Path
    state_dir: Path
    data_dir: Path
    reports_dir: Path
    status_path: Path
    market_map_path: Path
    fair_path: Path
    leaderboard_interval_s: float
    price_snapshot_interval_s: float
    env: Mapping[str, str] = field(repr=False)
    api_key: str = field(default="", repr=False)

    @property
    def intents_path(self) -> Path:
        return self.state_dir / "bridge" / "intents.jsonl"

    @property
    def has_api_key(self) -> bool:
        return bool(self.api_key)


def load_config(home: Path | None = None, env: Mapping[str, str] | None = None) -> BridgeConfig:
    """Load and validate the bridge configuration."""
    environ = dict(os.environ if env is None else env)
    root = Path(home or environ.get("SUSQ_HOME") or Path.cwd()).resolve()
    dotenv_file = root / ".env"
    merged: dict[str, str] = {}
    if dotenv_file.exists():
        merged.update({k: v for k, v in dotenv_values(dotenv_file).items() if v is not None})
    merged.update(environ)

    toml_path = Path(merged.get("SUSQ_CONFIG") or root / "config" / "susq.toml")
    with toml_path.open("rb") as fh:
        raw: dict[str, Any] = tomllib.load(fh)

    def path(value: str) -> Path:
        p = Path(value)
        return p if p.is_absolute() else root / p

    plat = raw["platform"]
    risk = raw["risk"]
    strategies = raw.get("strategies", {})
    paths = raw["paths"]
    platform = PlatformConfig(
        base_url=merged.get("SUSQ_BASE_URL") or plat["base_url"],
        alt_base_url=plat.get("alt_base_url", plat["base_url"]),
        tournament_slug=merged.get("SUSQ_TOURNAMENT_SLUG") or plat["tournament_slug"],
        request_timeout_ms=int(plat["request_timeout_ms"]),
        initial_rps=float(plat["initial_rps"]),
        min_rps=float(plat["min_rps"]),
        max_rps=float(plat["max_rps"]),
        order_budget_frac=float(plat["order_budget_frac"]),
    )
    calendar = CalendarConfig(
        open_utc=_utc(raw["calendar"]["open_utc"]),
        lock_utc=_utc(raw["calendar"]["lock_utc"]),
        last_order_utc=_utc(raw["calendar"]["last_order_utc"]),
    )
    if not calendar.open_utc < calendar.last_order_utc < calendar.lock_utc:
        raise ValueError("calendar must satisfy open < last_order < lock")
    racer = strategies.get("racer", {})
    sniper = strategies.get("sniper", {})
    state_dir = path(paths["state"])
    collateral = risk.get("use_collateral_buying_power", False)
    if collateral not in (True, False, "auto"):
        raise ValueError(
            f'[risk] use_collateral_buying_power must be true, false or "auto", not {collateral!r}'
        )
    return BridgeConfig(
        home=root,
        platform=platform,
        calendar=calendar,
        risk=RiskConfig(
            per_market_gross_frac=float(risk["per_market_gross_frac"]),
            total_gross_frac=float(risk["total_gross_frac"]),
            glitch_through_fair_pts=float(risk["glitch_through_fair_pts"]),
            glitch_max_frac=float(risk["glitch_max_frac"]),
            loss_per_hour_frac=float(risk["loss_per_hour_frac"]),
            reject_breaker_per_min=int(risk["reject_breaker_per_min"]),
            max_order_notional_frac=float(risk["max_order_notional_frac"]),
            clusters={k: float(v) for k, v in risk.get("clusters", {}).items()},
            use_collateral_buying_power=collateral is True,
            collateral_auto=collateral == "auto",
        ),
        oms=OmsConfig(
            default_ttl_s=float(raw["oms"]["default_ttl_s"]),
            reconcile_interval_s=float(raw["oms"]["reconcile_interval_s"]),
            idempotency_namespace=str(raw["oms"]["idempotency_namespace"]),
        ),
        fair=FairConfig(
            stale_after_s=float(raw["fair"]["stale_after_s"]),
            mapping_error_pts=float(raw["fair"]["mapping_error_pts"]),
        ),
        racer=RacerConfig(
            enabled=bool(racer.get("enabled", True)),
            poll_interval_s=float(racer.get("poll_interval_s", 3.0)),
            min_edge=float(racer.get("min_edge", 0.01)),
            max_bundle_notional_frac=float(racer.get("max_bundle_notional_frac", 0.01)),
            order_ttl_s=float(racer.get("order_ttl_s", 20)),
            refire_cooldown_s=float(racer.get("refire_cooldown_s", 30)),
            max_relationship_notional_frac=float(racer.get("max_relationship_notional_frac", 0.03)),
        ),
        sniper=SniperConfig(
            enabled=bool(sniper.get("enabled", True)),
            min_edge_pts=float(sniper.get("min_edge_pts", 3.5)),
            max_mistake_frac=float(sniper.get("max_mistake_frac", 0.01)),
            order_ttl_s=float(sniper.get("order_ttl_s", 30)),
        ),
        mode_default=str(raw.get("mode", {}).get("default", "paper")),
        lease_path=path(raw["lease"]["path"]),
        lease_ttl_s=float(raw["lease"]["ttl_s"]),
        lease_renew_s=float(raw["lease"]["renew_s"]),
        kill_file=path(raw["kill"]["file"]),
        go_live_file=path(raw["kill"]["go_live_file"]),
        journal_dir=path(paths["journal"]),
        state_dir=state_dir,
        data_dir=path(paths["data"]),
        reports_dir=path(paths["reports"]),
        status_path=path(paths["status"]),
        market_map_path=path(paths["market_map"]),
        fair_path=state_dir / "fair_live.json",
        leaderboard_interval_s=float(raw.get("telemetry", {}).get("leaderboard_interval_s", 3600)),
        price_snapshot_interval_s=float(raw.get("telemetry", {}).get("price_snapshot_interval_s", 60)),
        env=merged,
        api_key=merged.get("SUSQ_API_KEY", "").strip(),
    )
