// susq-needle: election-night county model as a standalone service on the NATS bus.
//
//   susq-needle --prior PRIOR.json [--nats URL] [--heartbeat-ms 1000]
//   susq-needle --prior PRIOR.json --replay BATCHES.jsonl [--out OUT.jsonl]
//
// Live mode subscribes to enr.county_results (binary CountyResultsBatch) and publishes
// needle.race_posteriors and needle.national_shift after every accepted batch, plus
// needle.heartbeat every --heartbeat-ms. SIGTERM/SIGINT shut down cleanly (exit 0).
// Replay mode reads one proto3-JSON CountyResultsBatch per line and writes one JSON line
// {"subject": ..., "payload": {...}} per output message; it never touches the network.
#include <algorithm>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <iostream>
#include <memory>
#include <string>

#include "nats_bus.hpp"
#include "susq/contracts.hpp"
#include "susq/needle/service.hpp"

#ifndef SUSQ_GIT_SHA
#define SUSQ_GIT_SHA "unknown"
#endif

namespace {

using susq::needle::NeedleService;

volatile std::sig_atomic_t g_stop = 0;

void OnSignal(int /*sig*/) { g_stop = 1; }

struct Args {
  std::string prior;
  std::string nats_url = "nats://127.0.0.1:4222";
  std::string replay;
  std::string out;
  long heartbeat_ms = 1000;
};

void Usage() {
  std::fprintf(stderr,
               "usage: susq-needle --prior PRIOR.json [--nats URL] [--heartbeat-ms N]\n"
               "       susq-needle --prior PRIOR.json --replay BATCHES.jsonl [--out OUT.jsonl]\n"
               "env: SUSQ_NATS_URL overrides the default NATS URL\n");
}

bool ParseArgs(int argc, char** argv, Args* args) {
  if (const char* env = std::getenv("SUSQ_NATS_URL")) args->nats_url = env;
  for (int i = 1; i < argc; ++i) {
    const std::string a = argv[i];
    auto value = [&](std::string* out) {
      if (i + 1 >= argc) return false;
      *out = argv[++i];
      return true;
    };
    std::string hb;
    if (a == "--prior") {
      if (!value(&args->prior)) return false;
    } else if (a == "--nats") {
      if (!value(&args->nats_url)) return false;
    } else if (a == "--replay") {
      if (!value(&args->replay)) return false;
    } else if (a == "--out") {
      if (!value(&args->out)) return false;
    } else if (a == "--heartbeat-ms") {
      if (!value(&hb)) return false;
      args->heartbeat_ms = std::strtol(hb.c_str(), nullptr, 10);
      if (args->heartbeat_ms <= 0 || args->heartbeat_ms > 2000) return false;  // contract: <= 2 s
    } else {
      return false;
    }
  }
  return !args->prior.empty();
}

void Log(const std::string& line) { std::fprintf(stderr, "susq-needle: %s\n", line.c_str()); }

std::string CountersLine(const NeedleService& svc) {
  const auto& c = svc.counters();
  const auto& m = svc.model().counters();
  return "batches_ok=" + std::to_string(c.batches_ok) + " malformed=" + std::to_string(c.malformed) +
         " bad_header=" + std::to_string(c.bad_header) + " results_accepted=" + std::to_string(m.accepted) +
         " duplicate=" + std::to_string(m.duplicate_batch) + " regression=" + std::to_string(m.regression) +
         " unknown_race=" + std::to_string(m.unknown_race) + " invalid=" + std::to_string(m.invalid);
}

int RunReplay(NeedleService& svc, const Args& args) {
  std::ifstream in(args.replay);
  if (!in) {
    Log("cannot open replay file " + args.replay);
    return 2;
  }
  std::ofstream file_out;
  if (!args.out.empty()) file_out.open(args.out);
  std::ostream& out = args.out.empty() ? std::cout : file_out;
  auto emit = [&](std::string_view subject, const google::protobuf::Message& msg) {
    out << "{\"subject\":\"" << subject << "\",\"payload\":" << susq::contracts::ToJson(msg).value_or("null")
        << "}\n";
  };
  std::string line;
  std::size_t line_no = 0;
  while (std::getline(in, line)) {
    ++line_no;
    if (line.find_first_not_of(" \t\r") == std::string::npos) continue;
    susq::v1::CountyResultsBatch batch;
    if (!susq::contracts::FromJson(line, &batch)) {
      Log("replay line " + std::to_string(line_no) + ": not a CountyResultsBatch, skipped");
      continue;
    }
    // Replays use the batch's own timestamp so outputs are reproducible.
    const int64_t ts = batch.has_header() ? batch.header().produced_at_ns() : 0;
    if (auto outputs = svc.HandleBatch(batch, ts)) {
      emit(susq::needle::kSubjectRacePosteriors, outputs->race_posteriors);
      emit(susq::needle::kSubjectNationalShift, outputs->national_shift);
    }
  }
  Log("replay done: " + CountersLine(svc));
  return 0;
}

int RunLive(NeedleService& svc, const Args& args) {
  std::string error;
  auto bus = susq::needle::NatsBus::Connect(args.nats_url, &error);
  if (!bus) {
    Log(error);
    return 2;
  }
  if (!bus->Subscribe(std::string(susq::needle::kSubjectCountyResults), &error)) {
    Log(error);
    return 2;
  }
  Log("started; nats=" + args.nats_url + " sha=" + SUSQ_GIT_SHA);

  const std::string subj_post(susq::needle::kSubjectRacePosteriors);
  const std::string subj_nat(susq::needle::kSubjectNationalShift);
  const std::string subj_hb(susq::needle::kSubjectHeartbeat);
  const int64_t hb_interval_ns = args.heartbeat_ms * 1'000'000;
  int64_t next_hb = 0;
  bool was_connected = bus->connected();
  while (g_stop == 0) {
    const int64_t now = susq::needle::NowNs();
    if (now >= next_hb) {
      const bool connected = bus->connected();
      if (connected != was_connected) Log(connected ? "bus connected" : "bus disconnected; retrying");
      was_connected = connected;
      svc.SetTransportHealthy(connected);
      bus->Publish(subj_hb, susq::contracts::Encode(svc.MakeHeartbeat(now)));
      next_hb = now + hb_interval_ns;
    }
    const int64_t wait_ms = std::clamp<int64_t>((next_hb - now) / 1'000'000, 1, 100);
    auto payload = bus->Next(std::chrono::milliseconds(wait_ms));
    if (!payload) continue;
    const uint64_t dropped_before = svc.counters().malformed + svc.counters().bad_header;
    if (auto outputs = svc.HandleBatchBytes(*payload, susq::needle::NowNs())) {
      bus->Publish(subj_post, susq::contracts::Encode(outputs->race_posteriors));
      bus->Publish(subj_nat, susq::contracts::Encode(outputs->national_shift));
    } else if (svc.counters().malformed + svc.counters().bad_header > dropped_before) {
      Log("dropped input message; " + CountersLine(svc));
    }
  }
  Log("stopping; " + CountersLine(svc));
  bus->Drain();
  bus.reset();
  nats_Close();
  return 0;
}

}  // namespace

int main(int argc, char** argv) {
  Args args;
  if (!ParseArgs(argc, argv, &args)) {
    Usage();
    return 64;
  }
  struct sigaction sa {};
  sa.sa_handler = OnSignal;
  sigemptyset(&sa.sa_mask);
  sigaction(SIGTERM, &sa, nullptr);
  sigaction(SIGINT, &sa, nullptr);

  try {
    susq::needle::ServiceOptions options;
    options.git_sha = SUSQ_GIT_SHA;
    // Live seq starts at wall-clock microseconds so it keeps increasing across restarts.
    options.seq_base = args.replay.empty() ? static_cast<uint64_t>(susq::needle::NowNs() / 1000) : 1;
    NeedleService svc(susq::needle::NeedleModel(susq::needle::LoadPrior(args.prior)), options);
    return args.replay.empty() ? RunLive(svc, args) : RunReplay(svc, args);
  } catch (const std::exception& e) {
    Log(std::string("fatal: ") + e.what());
    return 1;
  }
}
