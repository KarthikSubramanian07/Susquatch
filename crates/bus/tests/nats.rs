//! End-to-end tests against a real nats-server (skipped when none is installed).

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_nats::connection::State;
use common::NatsServer;
use futures::StreamExt;
use susq_bus::watchdog::NEEDLE_PRODUCER;
use susq_bus::{Bus, MonitorCfg, NeedleMonitor, NeedleSnapshot, NeedleStream, WatchdogCfg, subjects};
use susq_contracts::{SCHEMA_VERSION, decode, encode, v1};
use susq_core::{Clock, SystemClock};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_millis(300);
const N: u32 = 5;
const RACE: &str = "2026-SEN-OH-S";
const WAIT: Duration = Duration::from_secs(10);

/// Plays the needle: publishes stamped messages with per-stream seqs.
struct NeedleSim {
    client: async_nats::Client,
    seqs: [u64; 3],
}

impl NeedleSim {
    async fn connect(url: &str) -> Self {
        let options = async_nats::ConnectOptions::new()
            .name("needle-sim")
            .reconnect_delay_callback(|_| Duration::from_millis(50));
        let client = async_nats::connect_with_options(url, options)
            .await
            .expect("needle sim connects");
        Self { client, seqs: [0; 3] }
    }

    fn header(&mut self, stream: NeedleStream) -> Option<v1::Header> {
        let i = NeedleStream::ALL
            .iter()
            .position(|s| *s == stream)
            .expect("known stream");
        self.seqs[i] += 1;
        Some(v1::Header {
            schema_version: SCHEMA_VERSION,
            produced_at_ns: SystemClock.now_ns(),
            producer: Some(v1::Producer {
                name: NEEDLE_PRODUCER.to_owned(),
                git_sha: "sim".to_owned(),
            }),
            seq: self.seqs[i],
        })
    }

    /// Consume a seq without publishing it (a lost message).
    fn skip(&mut self, stream: NeedleStream) {
        self.header(stream);
    }

    async fn heartbeat(&mut self) {
        let msg = v1::Heartbeat {
            header: self.header(NeedleStream::Heartbeat),
            status: "ok".to_owned(),
            last_input_seq: 0,
        };
        self.raw(subjects::NEEDLE_HEARTBEAT, encode(&msg)).await;
    }

    async fn heartbeats(&mut self, count: u32) {
        for _ in 0..count {
            self.heartbeat().await;
        }
    }

    async fn posterior(&mut self, p_dem: f64) {
        let msg = v1::RacePosteriors {
            header: self.header(NeedleStream::Posteriors),
            races: vec![v1::RacePosterior {
                race_id: RACE.to_owned(),
                p_dem,
                p_rep: 1.0 - p_dem,
                margin_sd: 2.0,
                reported_frac: 0.3,
                ..Default::default()
            }],
        };
        self.raw(subjects::NEEDLE_RACE_POSTERIORS, encode(&msg)).await;
    }

    async fn raw(&self, subject: &'static str, payload: Vec<u8>) {
        self.client
            .publish(subject, payload.into())
            .await
            .expect("publish");
        self.client.flush().await.expect("flush");
    }
}

struct Harness {
    bus: Bus,
    monitor: NeedleMonitor,
    snaps: watch::Receiver<NeedleSnapshot>,
    sim: NeedleSim,
    cancel: CancellationToken,
}

impl Harness {
    async fn new(server: &NatsServer) -> Self {
        let bus = Bus::connect(&server.url(), "rust-live", "test")
            .await
            .expect("bus connects");
        let needle = bus.subscribe_needle().await.expect("subscribe needle");
        bus.flush().await.expect("subscriptions registered");
        let cancel = CancellationToken::new();
        let monitor = NeedleMonitor::spawn(
            needle,
            [RACE.to_owned()].into_iter().collect::<HashSet<_>>(),
            MonitorCfg::new(WatchdogCfg::new(TIMEOUT, N)),
            Arc::new(SystemClock),
            cancel.clone(),
        );
        let snaps = monitor.subscribe();
        let sim = NeedleSim::connect(&server.url()).await;
        Self {
            bus,
            monitor,
            snaps,
            sim,
            cancel,
        }
    }

    async fn wait_for(&mut self, what: &str, pred: impl FnMut(&NeedleSnapshot) -> bool) {
        tokio::time::timeout(WAIT, self.snaps.wait_for(pred))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for: {what}"))
            .expect("monitor alive");
    }

    async fn make_healthy(&mut self) {
        self.sim.heartbeats(N).await;
        self.wait_for("healthy", |s| s.healthy).await;
    }

    async fn shutdown(self) {
        self.cancel.cancel();
        self.monitor.join().await;
    }
}

macro_rules! server_or_skip {
    ($name:literal) => {
        match NatsServer::start($name) {
            Some(server) => server,
            None => return,
        }
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn county_results_round_trip() {
    let server = server_or_skip!("county_results_round_trip");
    let bus = Bus::connect(&server.url(), "rust-live", "abc123")
        .await
        .expect("connect");
    let raw = async_nats::connect(server.url()).await.expect("raw client");
    let mut sub = raw
        .subscribe(subjects::ENR_COUNTY_RESULTS)
        .await
        .expect("subscribe");
    raw.flush().await.expect("flush");

    let result = v1::CountyResult {
        race_id: RACE.to_owned(),
        state: "OH".to_owned(),
        county_fips: "39049".to_owned(),
        total_votes: 1234,
        ..Default::default()
    };
    let before = SystemClock.now_ns();
    assert_eq!(
        bus.publish_county_results(vec![result.clone()])
            .await
            .expect("publish 1"),
        1
    );
    assert_eq!(bus.publish_county_results(vec![]).await.expect("publish 2"), 2);

    for want_seq in [1, 2] {
        let msg = tokio::time::timeout(WAIT, sub.next())
            .await
            .expect("delivered")
            .expect("subscription open");
        let batch: v1::CountyResultsBatch = decode(&msg.payload).expect("decodes");
        let h = batch.header.expect("header");
        assert_eq!(h.seq, want_seq);
        assert_eq!(h.schema_version, SCHEMA_VERSION);
        assert!(h.produced_at_ns >= before);
        let producer = h.producer.expect("producer");
        assert_eq!(producer.name, "rust-live");
        assert_eq!(producer.git_sha, "abc123");
        if want_seq == 1 {
            assert_eq!(batch.results, vec![result.clone()]);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeats_make_monitor_healthy_after_n() {
    let server = server_or_skip!("heartbeats_make_monitor_healthy_after_n");
    let mut h = Harness::new(&server).await;
    assert!(!h.monitor.snapshot().healthy);

    h.sim.heartbeats(N - 1).await;
    h.wait_for("N-1 healthy messages", |s| s.healthy_streak == N - 1)
        .await;
    assert!(!h.monitor.snapshot().healthy, "not healthy before N");

    h.sim.posterior(0.64).await;
    h.wait_for("healthy after N", |s| s.healthy).await;
    let snap = h.monitor.snapshot();
    let p = snap
        .actionable_posterior(RACE, SystemClock.now_ns())
        .expect("posterior actionable");
    assert!((p.value.p_dem - 0.64).abs() < 1e-12);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_needle_goes_unhealthy_within_timeout() {
    let server = server_or_skip!("silent_needle_goes_unhealthy_within_timeout");
    let mut h = Harness::new(&server).await;
    h.make_healthy().await;
    let last_message = Instant::now();

    h.wait_for("unhealthy by silence", |s| !s.healthy).await;
    let elapsed = last_message.elapsed();
    let snap = h.monitor.snapshot();
    assert!(
        elapsed <= TIMEOUT + Duration::from_millis(200),
        "tripped after {elapsed:?}"
    );
    assert_eq!(snap.epoch, 1);
    assert!(snap.reason.as_deref().is_some_and(|r| r.contains("silent")));
    assert_eq!(snap.health.state, "down");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seq_gap_goes_unhealthy() {
    let server = server_or_skip!("seq_gap_goes_unhealthy");
    let mut h = Harness::new(&server).await;
    h.make_healthy().await;

    h.sim.skip(NeedleStream::Heartbeat);
    h.sim.heartbeat().await;
    h.wait_for("unhealthy by gap", |s| !s.healthy).await;
    let snap = h.monitor.snapshot();
    assert!(snap.reason.as_deref().is_some_and(|r| r.contains("seq gap")));
    assert_eq!(snap.errors_total, 0, "a gap is not a malformed message");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_payload_is_dropped_and_counted() {
    let server = server_or_skip!("malformed_payload_is_dropped_and_counted");
    let mut h = Harness::new(&server).await;
    h.make_healthy().await;
    h.sim.posterior(0.5).await;

    h.sim
        .raw(subjects::NEEDLE_RACE_POSTERIORS, vec![0x0f, 0xff, 0xff])
        .await;
    h.wait_for("error counted", |s| s.errors_total == 1).await;
    let snap = h.monitor.snapshot();
    assert!(!snap.healthy);
    assert_eq!(snap.errors_per_min, 1);
    assert!(snap.actionable_posteriors(SystemClock.now_ns()).is_none());
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ops_kill_is_delivered_even_when_malformed() {
    let server = server_or_skip!("ops_kill_is_delivered_even_when_malformed");
    let bus = Bus::connect(&server.url(), "rust-live", "test")
        .await
        .expect("connect");
    let mut kills = bus.subscribe_kill().await.expect("subscribe kill");
    bus.flush().await.expect("flush");

    bus.publish_kill("loss breaker", "watchdog")
        .await
        .expect("publish kill");
    let kill = tokio::time::timeout(WAIT, kills.next())
        .await
        .expect("kill delivered")
        .expect("open");
    assert_eq!(kill.reason, "loss breaker");
    assert_eq!(kill.issuer, "watchdog");
    assert_eq!(kill.header.map(|h| h.seq), Some(1));

    let raw = async_nats::connect(server.url()).await.expect("raw client");
    raw.publish(subjects::OPS_KILL, vec![0x0f, 0xff, 0xff].into())
        .await
        .expect("publish garbage");
    raw.flush().await.expect("flush");
    let kill = tokio::time::timeout(WAIT, kills.next())
        .await
        .expect("malformed kill still delivered")
        .expect("open");
    assert!(kill.reason.starts_with("malformed ops.kill payload"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_restart_reconnects_and_recovers_only_after_n() {
    let mut server = server_or_skip!("nats_restart_reconnects_and_recovers_only_after_n");
    let mut h = Harness::new(&server).await;
    h.make_healthy().await;

    server.stop();
    h.wait_for("unhealthy while NATS is down", |s| !s.healthy).await;
    assert!(
        h.bus.publish_county_results(vec![]).await.is_err(),
        "publishing while NATS is down fails within the op timeout"
    );

    server.restart();
    let deadline = Instant::now() + WAIT;
    while !(h.bus.is_connected() && h.sim.client.connection_state() == State::Connected) {
        assert!(Instant::now() < deadline, "clients did not reconnect");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.bus.flush().await.expect("resubscribed");

    h.sim.heartbeats(N - 1).await;
    h.wait_for("N-1 healthy after restart", |s| s.healthy_streak == N - 1)
        .await;
    assert!(!h.monitor.snapshot().healthy, "not healthy before N");
    h.sim.heartbeat().await;
    h.wait_for("healthy after N", |s| s.healthy).await;
    assert_eq!(h.monitor.snapshot().epoch, 1);
    h.shutdown().await;
}
