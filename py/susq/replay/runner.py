"""Replay runner: stream a replay at N× speed into (a) NATS `enr.county_results` or (b) a needle
backend in process, recording posteriors over time, optionally under chaos.

Chaos modes (all in replay seconds):
* **feed drop**: batches in `[drop_start_s, drop_start_s + drop_duration_s)` are not delivered;
  the publisher stays up and its heartbeats say `feed_ok = False` (the poller's sources are failing).
  When the feed comes back, the first batch carries everything that changed meanwhile, because the
  poller's next successful poll returns the sources' current cumulative state.
* **malformed injection**: every `malformed_every`-th delivery is preceded by a garbage payload.
* **publisher kill**: at `kill_at_s` the publisher dies (no batches, no heartbeats); after
  `restart_after_s` it restarts with a fresh `seq` (starting at 1) and first re-emits a snapshot of
  every series' latest cumulative result, exactly what a restarted `enr` poller does after
  re-polling its sources.

Heartbeats model the bus-level liveness signal (the needle heartbeat / enr health): one every
`heartbeat_s` while the publisher is alive.
"""

from __future__ import annotations

import asyncio
import enum
import time
from collections.abc import Iterator, Sequence
from dataclasses import dataclass, field

from google.protobuf.message import DecodeError

from susq.contracts import SCHEMA_VERSION, bus_pb2, common_pb2, decode, encode
from susq.replay.backends import NeedleBackend
from susq.replay.history import is_unit_key

NS = 1_000_000_000
GARBAGE = b"\xff\xff\xff\xff\x0f\x00garbage"
SUBJECT = "enr.county_results"


class InvalidPayloadError(ValueError):
    """A payload on the results stream that must be dropped (and counted)."""


def decode_batch(data: bytes) -> bus_pb2.CountyResultsBatch:
    """Decode and validate a bus payload, raising `InvalidPayloadError` for anything unusable."""
    try:
        batch = decode(bus_pb2.CountyResultsBatch, data)
    except DecodeError as e:
        raise InvalidPayloadError(f"protobuf: {e}") from e
    if not batch.HasField("header") or batch.header.schema_version != SCHEMA_VERSION:
        raise InvalidPayloadError("missing header or wrong schema version")
    for r in batch.results:
        if not is_unit_key(r.county_fips, r.state) or not r.race_id:
            raise InvalidPayloadError(f"bad result {r.race_id!r}/{r.county_fips!r}")
    return batch


@dataclass(frozen=True)
class ChaosConfig:
    drop_start_s: float | None = None
    drop_duration_s: float = 0.0
    malformed_every: int = 0
    kill_at_s: float | None = None
    restart_after_s: float = 60.0
    heartbeat_s: float = 30.0

    @property
    def active(self) -> bool:
        return self.drop_start_s is not None or self.malformed_every > 0 or self.kill_at_s is not None

    def describe(self) -> str:
        parts = []
        if self.drop_start_s is not None:
            parts.append(f"feed drop {self.drop_duration_s:.0f}s at t={self.drop_start_s:.0f}s")
        if self.malformed_every:
            parts.append(f"malformed payload every {self.malformed_every} deliveries")
        if self.kill_at_s is not None:
            parts.append(
                f"publisher killed at t={self.kill_at_s:.0f}s, restarted after {self.restart_after_s:.0f}s"
            )
        return "; ".join(parts) or "none"


class Kind(enum.Enum):
    BATCH = "batch"
    MALFORMED = "malformed"
    HEARTBEAT = "heartbeat"
    KILL = "kill"
    RESTART = "restart"


@dataclass(frozen=True)
class Delivery:
    t_s: float
    kind: Kind
    payload: bytes = b""
    feed_ok: bool = True


def _restamp(batch: bus_pb2.CountyResultsBatch, seq: int) -> bus_pb2.CountyResultsBatch:
    b = bus_pb2.CountyResultsBatch()
    b.CopyFrom(batch)
    b.header.seq = seq
    return b


def deliveries(
    batches: Sequence[bus_pb2.CountyResultsBatch], chaos: ChaosConfig | None = None
) -> Iterator[Delivery]:
    """What the consumer sees, in replay-time order, after chaos is applied."""
    chaos = chaos or ChaosConfig()
    if not batches:
        return
    next_hb = 0.0
    seq = 0
    n_delivered = 0
    killed_until: float | None = None
    restarted = False
    backlog: dict[tuple[str, str, int, str], bus_pb2.CountyResult] = {}
    latest: dict[tuple[str, str, int, str], bus_pb2.CountyResult] = {}
    producer = batches[0].header.producer

    def dropped(t: float) -> bool:
        return (
            chaos.drop_start_s is not None
            and chaos.drop_start_s <= t < chaos.drop_start_s + chaos.drop_duration_s
        )

    def publisher_down(t: float) -> bool:
        return chaos.kill_at_s is not None and chaos.kill_at_s <= t < chaos.kill_at_s + chaos.restart_after_s

    for batch in batches:
        t = batch.header.produced_at_ns / NS
        while next_hb <= t:
            if not publisher_down(next_hb):
                yield Delivery(next_hb, Kind.HEARTBEAT, feed_ok=not dropped(next_hb))
            next_hb += chaos.heartbeat_s
        # Published or not, the sources' state advances (a restarted poller re-reads it).
        for r in batch.results:
            latest[(r.race_id, r.county_fips, r.vote_type, r.source)] = r
        if chaos.kill_at_s is not None and killed_until is None and t >= chaos.kill_at_s:
            killed_until = chaos.kill_at_s + chaos.restart_after_s
            yield Delivery(chaos.kill_at_s, Kind.KILL)
        if killed_until is not None and not restarted and t >= killed_until:
            # Restart: fresh seq and a snapshot of everything the sources currently show.
            seq = 1
            snap = bus_pb2.CountyResultsBatch(
                header=common_pb2.Header(
                    schema_version=SCHEMA_VERSION,
                    produced_at_ns=int(killed_until * NS),
                    producer=producer,
                    seq=seq,
                )
            )
            snap.results.extend(latest.values())
            yield Delivery(killed_until, Kind.RESTART)
            yield Delivery(killed_until, Kind.BATCH, encode(snap))
            restarted = True
            continue
        if publisher_down(t):
            continue
        if dropped(t):
            for r in batch.results:
                backlog[(r.race_id, r.county_fips, r.vote_type, r.source)] = r
            continue
        if backlog:
            for r in batch.results:
                backlog.pop((r.race_id, r.county_fips, r.vote_type, r.source), None)
            caught_up = bus_pb2.CountyResultsBatch()
            caught_up.CopyFrom(batch)
            caught_up.results.extend(backlog.values())
            batch = caught_up
            backlog = {}
        n_delivered += 1
        if chaos.malformed_every and n_delivered % chaos.malformed_every == 0:
            yield Delivery(t, Kind.MALFORMED, GARBAGE)
        seq += 1
        yield Delivery(t, Kind.BATCH, encode(_restamp(batch, seq)))


@dataclass
class PosteriorSample:
    t_s: float  # replay time of the batch that produced it
    latency_s: float  # wall time from payload arrival to posterior
    seq: int
    races: dict[str, bus_pb2.RacePosterior]


@dataclass
class HealthEvent:
    t_s: float
    healthy_input: bool  # batch or heartbeat with feed_ok
    kind: Kind


@dataclass
class ReplayRecord:
    backend: str
    speed: float
    chaos: ChaosConfig
    samples: list[PosteriorSample] = field(default_factory=list)
    health: list[HealthEvent] = field(default_factory=list)
    batches_delivered: int = 0
    malformed_injected: int = 0
    malformed_rejected: int = 0
    seq_gaps: int = 0
    seq_resets: int = 0
    kills: int = 0
    restarts: int = 0
    wall_s: float = 0.0


class Consumer:
    """The needle-side consumer: validate, track `seq`, feed the backend, record posteriors."""

    def __init__(self, backend: NeedleBackend, record: ReplayRecord) -> None:
        self.backend = backend
        self.record = record
        self._last_seq: int | None = None

    def on_delivery(self, d: Delivery, arrived: float) -> None:
        rec = self.record
        if d.kind is Kind.HEARTBEAT:
            rec.health.append(HealthEvent(d.t_s, d.feed_ok, d.kind))
            return
        if d.kind is Kind.KILL:
            rec.kills += 1
            return
        if d.kind is Kind.RESTART:
            rec.restarts += 1
            return
        if d.kind is Kind.MALFORMED:
            rec.malformed_injected += 1
        try:
            batch = decode_batch(d.payload)
        except InvalidPayloadError:
            rec.malformed_rejected += 1
            return
        seq = batch.header.seq
        if self._last_seq is not None:
            if seq <= self._last_seq:
                rec.seq_resets += 1  # producer restarted; its snapshot re-establishes state
            elif seq != self._last_seq + 1:
                rec.seq_gaps += 1
        self._last_seq = seq
        rec.batches_delivered += 1
        rec.health.append(HealthEvent(d.t_s, True, d.kind))
        posteriors = self.backend.ingest(batch)
        rec.samples.append(
            PosteriorSample(
                t_s=d.t_s,
                latency_s=time.perf_counter() - arrived,
                seq=seq,
                races={p.race_id: p for p in posteriors.races},
            )
        )


def replay_in_process(
    batches: Sequence[bus_pb2.CountyResultsBatch],
    backend: NeedleBackend,
    *,
    speed: float = 0.0,
    chaos: ChaosConfig | None = None,
) -> ReplayRecord:
    """Feed a replay through `backend`. `speed` = replay seconds per wall second (0 = no sleeping)."""
    chaos = chaos or ChaosConfig()
    record = ReplayRecord(backend=getattr(backend, "name", type(backend).__name__), speed=speed, chaos=chaos)
    consumer = Consumer(backend, record)
    start = time.perf_counter()
    t_prev = 0.0
    for d in deliveries(batches, chaos):
        if speed > 0 and d.t_s > t_prev:
            time.sleep((d.t_s - t_prev) / speed)
        t_prev = max(t_prev, d.t_s)
        consumer.on_delivery(d, time.perf_counter())
    record.wall_s = time.perf_counter() - start
    return record


@dataclass
class PublishStats:
    published: int = 0
    malformed: int = 0
    reconnects: int = 0
    wall_s: float = 0.0


def _reanchor(data: bytes, t0_ns: int, speed: float) -> bytes:
    """Shift replay-relative timestamps onto the wall clock (at `speed`) before publishing."""
    b = decode(bus_pb2.CountyResultsBatch, data)
    now = time.time_ns()
    b.header.produced_at_ns = now
    for r in b.results:
        r.observed_at_ns = now
        rel = r.source_ts_ns / speed if speed > 0 else 0
        r.source_ts_ns = t0_ns + int(rel)
    return encode(b)


async def replay_to_nats(
    batches: Sequence[bus_pb2.CountyResultsBatch],
    url: str = "nats://127.0.0.1:4222",
    *,
    speed: float = 60.0,
    chaos: ChaosConfig | None = None,
    subject: str = SUBJECT,
) -> PublishStats:
    """Publish a replay to NATS in (compressed) real time. Heartbeats are not published here: on the
    real bus they come from the needle and the enr health, not from the results stream."""
    import nats  # local import: only this path needs a server

    chaos = chaos or ChaosConfig()
    stats = PublishStats()
    nc = await nats.connect(servers=[url], connect_timeout=2, max_reconnect_attempts=3)
    t0_ns = time.time_ns()
    start = time.perf_counter()
    t_prev = 0.0
    try:
        for d in deliveries(batches, chaos):
            if speed > 0 and d.t_s > t_prev:
                await asyncio.sleep((d.t_s - t_prev) / speed)
            t_prev = max(t_prev, d.t_s)
            if d.kind is Kind.KILL:
                await nc.close()  # abrupt: no drain, like a crashed process
            elif d.kind is Kind.RESTART:
                nc = await nats.connect(servers=[url], connect_timeout=2, max_reconnect_attempts=3)
                stats.reconnects += 1
            elif d.kind is Kind.MALFORMED:
                await nc.publish(subject, d.payload)
                stats.malformed += 1
            elif d.kind is Kind.BATCH:
                await nc.publish(subject, _reanchor(d.payload, t0_ns, speed))
                stats.published += 1
        await nc.flush()
    finally:
        if not nc.is_closed:
            await nc.drain()
    stats.wall_s = time.perf_counter() - start
    return stats
