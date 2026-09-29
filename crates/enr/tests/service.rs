//! End-to-end service tests against a local mock HTTP server: conditional GETs, Clarity version
//! polling, and faults (500s, timeouts, malformed payloads, version regressions, consumer drop).

use std::time::Duration;

use chrono_tz::Tz;
use susq_contracts::v1::CountyResultsBatch;
use susq_enr::adapters::clarity::{ClarityDetail, ClaritySource};
use susq_enr::adapters::{AdapterContext, ResultsSource, ca_sos::CaSosSource};
use susq_enr::config::{RaceMap, SourceTiming};
use susq_enr::http::HttpClient;
use susq_enr::model::PRODUCER;
use susq_enr::service::{EnrHealth, ServiceConfig, SourceTask, spawn};
use tokio::sync::{mpsc, watch};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture")
}

fn fast() -> SourceTiming {
    SourceTiming {
        interval: Duration::from_millis(40),
        jitter_frac: 0.1,
        timeout: Duration::from_millis(300),
        max_backoff: Duration::from_millis(200),
    }
}

fn ca_source(id: &str, urls: Vec<String>, timing: SourceTiming) -> SourceTask {
    let ctx = AdapterContext {
        source_id: id.into(),
        state: "CA".into(),
        races: RaceMap::from_pairs([("Governor", "2026-GOV-CA")]),
        tz: Tz::America__Los_Angeles,
    };
    let http = HttpClient::new(timing.timeout, "susq-test").expect("client");
    SourceTask {
        source: Box::new(CaSosSource::new(ctx, urls, http)),
        timing,
    }
}

fn ca_body(county: &str, dem: u64, rep: u64) -> String {
    format!(
        r#"[{{"raceTitle":"Governor - {county} County Results","Reporting":"10% (1 of 10) precincts reporting",
        "ReportingTime":"November 3, 2026, 9:00 p.m.","candidates":[
        {{"Name":"Xavier Becerra","Party":"Dem","Votes":"{dem}"}},{{"Name":"Steve Hilton","Party":"Rep","Votes":"{rep}"}}]}}]"#
    )
}

async fn next_batch(rx: &mut mpsc::Receiver<CountyResultsBatch>) -> CountyResultsBatch {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("batch in time")
        .expect("channel open")
}

async fn wait_health(h: &mut watch::Receiver<EnrHealth>, pred: impl Fn(&EnrHealth) -> bool) -> EnrHealth {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if pred(&h.borrow_and_update()) {
                return h.borrow().clone();
            }
            h.changed().await.expect("health channel");
        }
    })
    .await
    .expect("health condition in time")
}

#[tokio::test]
async fn ca_sos_uses_conditional_get_and_emits_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/media/governor.json"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/media/governor.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .set_body_bytes(fixture("ca_sos_governor_testdata_2026-09-28.json")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (tx, mut rx) = mpsc::channel(16);
    let src = ca_source(
        "ca_sos",
        vec![format!("{}/media/governor.json", server.uri())],
        fast(),
    );
    let mut handle = spawn(ServiceConfig::default(), vec![src], tx, None);
    let batch = next_batch(&mut rx).await;
    let h = batch.header.as_ref().expect("header");
    assert_eq!(h.seq, 1);
    assert_eq!(h.schema_version, susq_contracts::SCHEMA_VERSION);
    assert_eq!(h.producer.as_ref().expect("producer").name, PRODUCER);
    assert_eq!(batch.results.len(), 58);
    assert!(batch.results.iter().all(|r| r.batch_id.len() == 32));

    let health = wait_health(&mut handle.health, |h| h.sources["ca_sos"].not_modified >= 3).await;
    assert_eq!(health.last_seq, 1, "304s produce no batches");
    assert!(
        tokio::time::timeout(Duration::from_millis(150), rx.recv())
            .await
            .is_err()
    );
    handle.shutdown().await;
    server.verify().await;
}

#[tokio::test]
async fn clarity_fetches_only_on_version_change_and_refuses_regressions() {
    let server = MockServer::start().await;
    let ver = |v: &str, n: u64| {
        Mock::given(method("GET"))
            .and(path("/SC/122436/current_ver.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string(v))
            .up_to_n_times(n)
    };
    ver("100\n", 3).mount(&server).await;
    ver("99\n", 2).mount(&server).await;
    ver("101\n", u64::MAX).mount(&server).await;
    let zip = fixture("clarity_sc_2024_detailxml.zip");
    Mock::given(path("/SC/122436/100/reports/detailxml.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip.clone()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/SC/122436/101/reports/detailxml.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip))
        .expect(1)
        .mount(&server)
        .await;

    let ctx = AdapterContext {
        source_id: "clarity:SC".into(),
        state: "SC".into(),
        races: RaceMap::from_pairs([("President and Vice President", "2024-PRES-SC")]),
        tz: Tz::America__New_York,
    };
    let http = HttpClient::new(Duration::from_secs(2), "susq-test").expect("client");
    let source = ClaritySource::new(
        ctx,
        format!("{}/SC/122436", server.uri()),
        ClarityDetail::Xml,
        http,
    );
    assert_eq!(source.id(), "clarity:SC");
    let (tx, mut rx) = mpsc::channel(16);
    let mut handle = spawn(
        ServiceConfig::default(),
        vec![SourceTask {
            source: Box::new(source),
            timing: fast(),
        }],
        tx,
        None,
    );

    let batch = next_batch(&mut rx).await;
    assert_eq!(batch.results.len(), 46 * 4);
    let health = wait_health(&mut handle.health, |h| {
        let s = &h.sources["clarity:SC"];
        s.version_regressions >= 1 && s.ok_polls >= 4
    })
    .await;
    assert!(
        health.sources["clarity:SC"].not_modified >= 2,
        "same version twice -> no refetch"
    );
    // Version 101 carries identical numbers: fetched once (verified below), deduped, no batch.
    let health = wait_health(&mut handle.health, |h| h.pipeline.duplicates >= 46 * 4).await;
    assert_eq!(health.last_seq, 1);
    handle.shutdown().await;
    server.verify().await;
}

#[tokio::test]
async fn faults_are_counted_and_isolated_per_source() {
    let server = MockServer::start().await;
    Mock::given(path("/down.json"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(path("/slow.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(ca_body("Alpine", 1, 1))
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&server)
        .await;
    Mock::given(path("/flaky.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"raceTitle\": [truncated"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(path("/flaky.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ca_body("Alameda", 900, 700)))
        .mount(&server)
        .await;

    let (tx, mut rx) = mpsc::channel(16);
    let sources = vec![
        ca_source("down", vec![format!("{}/down.json", server.uri())], fast()),
        ca_source("slow", vec![format!("{}/slow.json", server.uri())], fast()),
        ca_source("flaky", vec![format!("{}/flaky.json", server.uri())], fast()),
    ];
    let mut handle = spawn(ServiceConfig::default(), sources, tx, None);

    let batch = next_batch(&mut rx).await;
    assert_eq!(batch.results[0].source, "flaky");
    assert_eq!(batch.results[0].county_fips, "06001");
    let h = wait_health(&mut handle.health, |h| {
        h.sources["down"].failed_polls >= 2
            && h.sources["slow"].failed_polls >= 1
            && h.sources["flaky"].malformed_payloads >= 2
    })
    .await;
    assert!(
        h.sources["down"]
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("500")
    );
    assert!(
        h.sources["slow"]
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("timed out")
    );
    assert!(h.sources["down"].consecutive_failures >= 2);
    assert_eq!(h.sources["flaky"].consecutive_failures, 0);
    handle.shutdown().await;
}

#[tokio::test]
async fn disagreeing_sources_mark_the_race_disputed() {
    let server = MockServer::start().await;
    Mock::given(path("/a.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ca_body("Alameda", 60_000, 40_000)))
        .mount(&server)
        .await;
    Mock::given(path("/b.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ca_body("Alameda", 40_000, 60_000)))
        .mount(&server)
        .await;
    let (tx, mut rx) = mpsc::channel(16);
    let sources = vec![
        ca_source("feed_a", vec![format!("{}/a.json", server.uri())], fast()),
        ca_source("feed_b", vec![format!("{}/b.json", server.uri())], fast()),
    ];
    let mut handle = spawn(ServiceConfig::default(), sources, tx, None);
    let _ = next_batch(&mut rx).await;
    let h = wait_health(&mut handle.health, |h| !h.disputed_races.is_empty()).await;
    assert!(h.disputed_races.contains("2026-GOV-CA"));
    assert!(h.dispute_details["2026-GOV-CA"][0].contains("06001"));
    handle.shutdown().await;
}

#[tokio::test]
async fn dropping_the_consumer_stops_every_task() {
    let server = MockServer::start().await;
    Mock::given(path("/a.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ca_body("Alameda", 1, 2)))
        .mount(&server)
        .await;
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let handle = spawn(
        ServiceConfig::default(),
        vec![ca_source("a", vec![format!("{}/a.json", server.uri())], fast())],
        tx,
        None,
    );
    let token = handle.cancel_token();
    tokio::time::timeout(Duration::from_secs(5), token.cancelled())
        .await
        .expect("service stops itself");
    handle.shutdown().await;
}

#[tokio::test]
async fn archive_captures_raw_and_batches() {
    let server = MockServer::start().await;
    Mock::given(path("/a.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ca_body("Alameda", 5, 6)))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (tx, mut rx) = mpsc::channel(16);
    let archive = susq_enr::archive::Archive::new(dir.path());
    let handle = spawn(
        ServiceConfig::default(),
        vec![ca_source(
            "ca_sos",
            vec![format!("{}/a.json", server.uri())],
            fast(),
        )],
        tx,
        Some(archive),
    );
    let _ = next_batch(&mut rx).await;
    handle.shutdown().await;
    let day = std::fs::read_dir(dir.path().join("ca_sos"))
        .expect("source dir")
        .next()
        .expect("day dir")
        .expect("entry")
        .path();
    let batches = std::fs::read_to_string(day.join("batches.jsonl")).expect("batches");
    let parsed: CountyResultsBatch =
        serde_json::from_str(batches.lines().next().expect("line")).expect("proto3 json");
    assert_eq!(parsed.results[0].county_fips, "06001");
    let raws: Vec<_> = std::fs::read_dir(day.join("raw")).expect("raw dir").collect();
    assert_eq!(raws.len(), 1, "identical bodies are stored once");
}
