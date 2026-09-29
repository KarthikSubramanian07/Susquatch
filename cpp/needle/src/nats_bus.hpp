// Thin RAII wrapper over the NATS C client (cnats) for the needle service.
// Connects in the background with unlimited reconnects, so the service keeps running (and
// reports itself degraded) while the server is down.
#pragma once

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <deque>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>

#include <nats/nats.h>

namespace susq::needle {

class NatsBus {
 public:
  /// Starts connecting to `url`; returns nullptr (and sets `error`) only on configuration errors.
  static std::unique_ptr<NatsBus> Connect(const std::string& url, std::string* error);
  ~NatsBus();
  NatsBus(const NatsBus&) = delete;
  NatsBus& operator=(const NatsBus&) = delete;

  /// Subscribes; received payloads are queued for Next().
  bool Subscribe(const std::string& subject, std::string* error);

  /// Publishes a payload; false if the client refused it (e.g. reconnect buffer full).
  bool Publish(const std::string& subject, const std::string& payload);

  /// Waits up to `timeout` for the next queued payload.
  std::optional<std::string> Next(std::chrono::milliseconds timeout);

  /// Flushes pending publishes (bounded wait), then unsubscribes.
  void Drain();

  bool connected() const { return connected_.load(); }
  uint64_t dropped_inbound() const { return dropped_inbound_.load(); }

 private:
  NatsBus() = default;
  static void OnMessage(natsConnection* nc, natsSubscription* sub, natsMsg* msg, void* closure);
  static void OnConnected(natsConnection* nc, void* closure);
  static void OnDisconnected(natsConnection* nc, void* closure);

  static constexpr std::size_t kMaxQueued = 100000;

  natsConnection* conn_ = nullptr;
  natsSubscription* sub_ = nullptr;
  std::atomic<bool> connected_{false};
  std::atomic<uint64_t> dropped_inbound_{0};
  std::mutex mu_;
  std::condition_variable cv_;
  std::deque<std::string> queue_;
};

}  // namespace susq::needle
