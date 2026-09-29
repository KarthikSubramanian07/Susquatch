// End-to-end: a real nats-server, the real susq-needle binary and a cnats client.
// Skipped (with a message) when nats-server is not installed; set SUSQ_NATS_SERVER to point at it.
#include <gtest/gtest.h>

#include <netinet/in.h>
#include <signal.h>
#include <spawn.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <chrono>
#include <cstdlib>
#include <fstream>
#include <optional>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#include <nats/nats.h>

#include "susq/contracts.hpp"

extern char** environ;

namespace {

using namespace std::chrono_literals;

std::string FindNatsServer() {
  if (const char* env = std::getenv("SUSQ_NATS_SERVER")) return env;
  std::vector<std::string> candidates{"/opt/homebrew/bin/nats-server", "/usr/local/bin/nats-server"};
  if (const char* path = std::getenv("PATH")) {
    std::stringstream ss(path);
    std::string dir;
    while (std::getline(ss, dir, ':')) candidates.push_back(dir + "/nats-server");
  }
  for (const auto& c : candidates) {
    if (access(c.c_str(), X_OK) == 0) return c;
  }
  return "";
}

int FreePort() {
  const int fd = socket(AF_INET, SOCK_STREAM, 0);
  sockaddr_in addr{};
  addr.sin_family = AF_INET;
  addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  addr.sin_port = 0;
  bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr));
  socklen_t len = sizeof(addr);
  getsockname(fd, reinterpret_cast<sockaddr*>(&addr), &len);
  close(fd);
  return ntohs(addr.sin_port);
}

// A spawned child process, killed on destruction.
class Child {
 public:
  explicit Child(std::vector<std::string> argv) {
    std::vector<char*> args;
    for (auto& a : argv) args.push_back(a.data());
    args.push_back(nullptr);
    if (posix_spawn(&pid_, args[0], nullptr, nullptr, args.data(), environ) != 0) pid_ = -1;
  }
  ~Child() {
    if (pid_ > 0 && !exited_) {
      kill(pid_, SIGKILL);
      waitpid(pid_, nullptr, 0);
    }
  }
  Child(const Child&) = delete;
  Child& operator=(const Child&) = delete;

  bool ok() const { return pid_ > 0; }
  void Signal(int sig) const { kill(pid_, sig); }

  /// Waits for exit; returns the exit status, or nullopt on timeout / abnormal exit.
  std::optional<int> Wait(std::chrono::milliseconds timeout) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (std::chrono::steady_clock::now() < deadline) {
      int status = 0;
      if (waitpid(pid_, &status, WNOHANG) == pid_) {
        exited_ = true;
        return WIFEXITED(status) ? std::optional<int>(WEXITSTATUS(status)) : std::nullopt;
      }
      std::this_thread::sleep_for(20ms);
    }
    return std::nullopt;
  }

 private:
  pid_t pid_ = -1;
  bool exited_ = false;
};

std::string ReadFile(const std::string& path) {
  std::ifstream in(path, std::ios::binary);
  std::ostringstream buf;
  buf << in.rdbuf();
  return buf.str();
}

class Client {
 public:
  explicit Client(const std::string& url) {
    natsOptions* opts = nullptr;
    natsOptions_Create(&opts);
    natsOptions_SetURL(opts, url.c_str());
    natsOptions_SetMaxReconnect(opts, -1);
    natsOptions_SetReconnectWait(opts, 100);
    for (int i = 0; i < 50 && conn_ == nullptr; ++i) {
      if (natsConnection_Connect(&conn_, opts) != NATS_OK) {
        conn_ = nullptr;
        std::this_thread::sleep_for(100ms);
      }
    }
    natsOptions_Destroy(opts);
    if (conn_ != nullptr) natsConnection_SubscribeSync(&sub_, conn_, "needle.>");
    if (conn_ != nullptr) natsConnection_FlushTimeout(conn_, 2000);
  }
  ~Client() {
    if (sub_ != nullptr) natsSubscription_Destroy(sub_);
    if (conn_ != nullptr) natsConnection_Destroy(conn_);
  }
  bool ok() const { return conn_ != nullptr && sub_ != nullptr; }

  void Publish(const std::string& subject, const std::string& payload) {
    natsConnection_Publish(conn_, subject.c_str(), payload.data(), static_cast<int>(payload.size()));
    natsConnection_FlushTimeout(conn_, 2000);
  }

  /// Next message on `subject` within `timeout` (other subjects are skipped).
  std::optional<std::string> Next(const std::string& subject, std::chrono::milliseconds timeout) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (std::chrono::steady_clock::now() < deadline) {
      natsMsg* msg = nullptr;
      if (natsSubscription_NextMsg(&msg, sub_, 100) != NATS_OK) continue;
      const std::string subj = natsMsg_GetSubject(msg);
      std::string data(natsMsg_GetData(msg), static_cast<std::size_t>(natsMsg_GetDataLength(msg)));
      natsMsg_Destroy(msg);
      if (subj == subject) return data;
    }
    return std::nullopt;
  }

 private:
  natsConnection* conn_ = nullptr;
  natsSubscription* sub_ = nullptr;
};

std::optional<susq::v1::Heartbeat> NextHeartbeat(Client& client, std::chrono::milliseconds timeout) {
  auto bytes = client.Next("needle.heartbeat", timeout);
  if (!bytes) return std::nullopt;
  susq::v1::Heartbeat hb;
  if (!susq::contracts::Decode(*bytes, &hb)) return std::nullopt;
  return hb;
}

TEST(NatsIntegration, RoundTripRejectsGarbageReconnectsAndStopsCleanly) {
  const std::string server = FindNatsServer();
  if (server.empty()) GTEST_SKIP() << "nats-server not found (brew install nats-server, or set SUSQ_NATS_SERVER)";
  const std::string port = std::to_string(FreePort());
  const std::string url = "nats://127.0.0.1:" + port;
  auto nats = std::make_unique<Child>(std::vector<std::string>{server, "-a", "127.0.0.1", "-p", port});
  ASSERT_TRUE(nats->ok());
  Child needle({SUSQ_NEEDLE_BIN, "--prior", std::string(SUSQ_FIXTURES_DIR) + "/needle_prior.json", "--nats", url,
                "--heartbeat-ms", "200"});
  ASSERT_TRUE(needle.ok());
  Client client(url);
  ASSERT_TRUE(client.ok()) << "could not connect to " << url;

  // Heartbeats flow with a proper header and increasing seq.
  auto hb1 = NextHeartbeat(client, 5s);
  auto hb2 = NextHeartbeat(client, 2s);
  ASSERT_TRUE(hb1 && hb2);
  EXPECT_EQ(hb1->header().producer().name(), "needle");
  EXPECT_EQ(hb1->header().schema_version(), susq::contracts::kSchemaVersion);
  EXPECT_GT(hb2->header().seq(), hb1->header().seq());
  EXPECT_EQ(hb2->status(), "ok");

  // A county batch produces posteriors and a national shift.
  client.Publish("enr.county_results", ReadFile(std::string(SUSQ_FIXTURES_DIR) + "/county_results_batch.binpb"));
  auto post_bytes = client.Next("needle.race_posteriors", 3s);
  ASSERT_TRUE(post_bytes.has_value());
  susq::v1::RacePosteriors post;
  ASSERT_TRUE(susq::contracts::Decode(*post_bytes, &post));
  ASSERT_EQ(post.races_size(), 1);
  EXPECT_EQ(post.races(0).race_id(), "2026-SEN-OH-S");
  EXPECT_EQ(post.header().producer().name(), "needle");
  auto shift_bytes = client.Next("needle.national_shift", 3s);
  ASSERT_TRUE(shift_bytes.has_value());
  auto hb3 = NextHeartbeat(client, 2s);
  ASSERT_TRUE(hb3);
  EXPECT_EQ(hb3->last_input_seq(), 42U);

  // Garbage is dropped, reported in the heartbeat, and the service keeps running.
  client.Publish("enr.county_results", std::string("\xff\xff\xff\xff", 4));
  bool saw_degraded = false;
  for (int i = 0; i < 10 && !saw_degraded; ++i) {
    auto hb = NextHeartbeat(client, 1s);
    saw_degraded = hb && hb->status() == "degraded:rejected_inputs";
  }
  EXPECT_TRUE(saw_degraded);

  // Restart the bus on the same port: needle reconnects and heartbeats resume.
  nats->Signal(SIGTERM);
  ASSERT_TRUE(nats->Wait(5s).has_value());
  nats = std::make_unique<Child>(std::vector<std::string>{server, "-a", "127.0.0.1", "-p", port});
  ASSERT_TRUE(nats->ok());
  // Heartbeats published while disconnected are buffered and flushed on reconnect, so prove the
  // round trip instead: a new batch must come back as posteriors (needle resubscribed).
  const std::string batch = ReadFile(std::string(SUSQ_FIXTURES_DIR) + "/county_results_batch.binpb");
  std::optional<std::string> reposted;
  for (int attempt = 0; attempt < 30 && !reposted; ++attempt) {
    client.Publish("enr.county_results", batch);
    reposted = client.Next("needle.race_posteriors", 500ms);
  }
  ASSERT_TRUE(reposted.has_value()) << "no posteriors after NATS restart";
  susq::v1::RacePosteriors post2;
  ASSERT_TRUE(susq::contracts::Decode(*reposted, &post2));
  EXPECT_GT(post2.header().seq(), post.header().seq());
  // Heartbeats generated after the round trip report a healthy bus (the rejected-input window
  // from the garbage above may still be open).
  auto healthy = NextHeartbeat(client, 2s);
  ASSERT_TRUE(healthy.has_value());
  EXPECT_NE(healthy->status(), "degraded:bus_disconnected");
  EXPECT_EQ(healthy->last_input_seq(), 42U);

  // SIGTERM: clean exit.
  needle.Signal(SIGTERM);
  const auto code = needle.Wait(5s);
  ASSERT_TRUE(code.has_value());
  EXPECT_EQ(*code, 0);
}

TEST(NatsIntegration, ReplayModeNeedsNoBus) {
  const std::string dir = ::testing::TempDir();
  const std::string in = dir + "/needle_replay_in.jsonl";
  const std::string out = dir + "/needle_replay_out.jsonl";
  {
    std::ofstream f(in);
    std::string json = ReadFile(std::string(SUSQ_FIXTURES_DIR) + "/county_results_batch.json");
    json.erase(std::remove(json.begin(), json.end(), '\n'), json.end());
    f << json << "\n\nnot json\n" << json << "\n";
  }
  Child replay({SUSQ_NEEDLE_BIN, "--prior", std::string(SUSQ_FIXTURES_DIR) + "/needle_prior.json", "--replay", in,
                "--out", out});
  ASSERT_TRUE(replay.ok());
  const auto code = replay.Wait(10s);
  ASSERT_TRUE(code.has_value());
  EXPECT_EQ(*code, 0);
  std::ifstream f(out);
  std::string line;
  std::vector<std::string> lines;
  while (std::getline(f, line)) lines.push_back(line);
  ASSERT_EQ(lines.size(), 4U);  // two batches x (posteriors, national shift); bad line skipped
  EXPECT_NE(lines[0].find("\"subject\":\"needle.race_posteriors\""), std::string::npos);
  EXPECT_NE(lines[1].find("\"subject\":\"needle.national_shift\""), std::string::npos);
  EXPECT_NE(lines[0].find("2026-SEN-OH-S"), std::string::npos);
}

TEST(NatsIntegration, RejectsBadArguments) {
  Child no_prior({SUSQ_NEEDLE_BIN, "--nats", "nats://127.0.0.1:1"});
  const auto code = no_prior.Wait(5s);
  ASSERT_TRUE(code.has_value());
  EXPECT_EQ(*code, 64);
  Child missing({SUSQ_NEEDLE_BIN, "--prior", "/nonexistent/prior.json", "--replay", "/dev/null"});
  const auto code2 = missing.Wait(5s);
  ASSERT_TRUE(code2.has_value());
  EXPECT_EQ(*code2, 1);
}

}  // namespace
