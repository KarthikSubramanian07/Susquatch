"""EXP-014 contender drill: both profiles over seeded market models, invariants and the report."""

from __future__ import annotations

import pytest
from susq.replay.contender import ContenderConfig, render_contender, run_seeds, simulate_contender
from susq.replay.needle_ref import ReferenceNeedle
from susq.replay.runner import ChaosConfig, replay_in_process
from susq.replay.spec import load_spec, materialize

from .conftest import SAMPLE_SPEC


@pytest.fixture(scope="module")
def replayed():
    batches, meta, prior = materialize(load_spec(SAMPLE_SPEC))
    needle = ReferenceNeedle()
    needle.load_prior(prior)
    return replay_in_process(batches, needle, chaos=ChaosConfig()), prior, meta.races


def test_runs_are_seeded_and_reproducible(replayed) -> None:
    record, prior, truths = replayed
    a = simulate_contender(record, prior, truths, ContenderConfig(), seed=3)
    b = simulate_contender(record, prior, truths, ContenderConfig(), seed=3)
    assert a.final_multiple == b.final_multiple
    assert a.path == b.path
    assert a.path[0][1] == pytest.approx(0.98, abs=0.05), "starts at the configured multiple"


def test_orders_never_exceed_depth_and_nothing_trades_on_a_healthy_free_record(replayed) -> None:
    record, prior, truths = replayed
    drill = run_seeds(record, prior, truths, ContenderConfig(), seeds=8)
    for r in drill.contender + drill.default:
        assert r.max_order_over_depth <= 1.0 + 1e-9
        assert r.refused_unhealthy == 0, "no chaos: the watchdog never had to stop anything"
        assert r.final_multiple >= 0.0
    assert any(r.buys for r in drill.contender), "a lagging book leaves edge to buy"


def test_contender_holds_to_the_call_and_default_rolls_small(replayed) -> None:
    record, prior, truths = replayed
    drill = run_seeds(record, prior, truths, ContenderConfig(), seeds=8)
    whys = {roll.why for r in drill.contender for roll in r.rolls}
    assert whys <= {"exit_bid", "called"}
    assert all(roll.why == "roll" for r in drill.default for roll in r.rolls)
    assert all(roll.bid >= 0.93 for r in drill.contender for roll in r.rolls if roll.why == "exit_bid")


def test_nothing_is_sent_while_the_input_is_down() -> None:
    batches, meta, prior = materialize(load_spec(SAMPLE_SPEC))
    needle = ReferenceNeedle()
    needle.load_prior(prior)
    # The feed drops for an hour mid-night: orders the profile wants then are refused, not sent.
    record = replay_in_process(batches, needle, chaos=ChaosConfig(drop_start_s=1800, drop_duration_s=3600))
    r = simulate_contender(record, prior, meta.races, ContenderConfig(), seed=1)
    assert r.refused_unhealthy > 0
    healthy = replay_in_process(batches, needle, chaos=ChaosConfig())
    assert simulate_contender(healthy, prior, meta.races, ContenderConfig(), seed=1).refused_unhealthy == 0


def test_report_carries_the_distribution_and_pass_bars(replayed) -> None:
    record, prior, truths = replayed
    cfg = ContenderConfig()
    drill = run_seeds(record, prior, truths, cfg, seeds=4)
    text = render_contender(drill, title="t", replay="spec", backend_note="python-reference", cfg=cfg)
    for needle in ("Contender", "Default", "runs ≥ 10x", "max order / depth", "Best contender run"):
        assert needle in text
