#include "nats_bus.hpp"

namespace susq::needle {
namespace {

std::string StatusText(natsStatus s) { return natsStatus_GetText(s); }

}  // namespace

std::unique_ptr<NatsBus> NatsBus::Connect(const std::string& url, std::string* error) {
  std::unique_ptr<NatsBus> bus(new NatsBus());
  natsOptions* opts = nullptr;
  natsStatus s = natsOptions_Create(&opts);
  if (s == NATS_OK) s = natsOptions_SetURL(opts, url.c_str());
  if (s == NATS_OK) s = natsOptions_SetName(opts, "susq-needle");
  if (s == NATS_OK) s = natsOptions_SetMaxReconnect(opts, -1);  // forever
  if (s == NATS_OK) s = natsOptions_SetReconnectWait(opts, 500);
  if (s == NATS_OK) s = natsOptions_SetReconnectedCB(opts, &NatsBus::OnConnected, bus.get());
  if (s == NATS_OK) s = natsOptions_SetDisconnectedCB(opts, &NatsBus::OnDisconnected, bus.get());
  // With a connected callback, Connect returns immediately and keeps retrying in the background.
  if (s == NATS_OK) s = natsOptions_SetRetryOnFailedConnect(opts, true, &NatsBus::OnConnected, bus.get());
  if (s == NATS_OK) {
    s = natsConnection_Connect(&bus->conn_, opts);
    if (s == NATS_OK) {
      bus->connected_ = true;
    } else if (s == NATS_NOT_YET_CONNECTED) {
      s = NATS_OK;
    }
  }
  natsOptions_Destroy(opts);
  if (s != NATS_OK) {
    if (error != nullptr) *error = "nats connect to " + url + ": " + StatusText(s);
    return nullptr;
  }
  return bus;
}

NatsBus::~NatsBus() {
  if (sub_ != nullptr) natsSubscription_Destroy(sub_);
  if (conn_ != nullptr) natsConnection_Destroy(conn_);
}

bool NatsBus::Subscribe(const std::string& subject, std::string* error) {
  const natsStatus s = natsConnection_Subscribe(&sub_, conn_, subject.c_str(), &NatsBus::OnMessage, this);
  if (s != NATS_OK && error != nullptr) *error = "nats subscribe " + subject + ": " + StatusText(s);
  return s == NATS_OK;
}

bool NatsBus::Publish(const std::string& subject, const std::string& payload) {
  return natsConnection_Publish(conn_, subject.c_str(), payload.data(), static_cast<int>(payload.size())) == NATS_OK;
}

std::optional<std::string> NatsBus::Next(std::chrono::milliseconds timeout) {
  std::unique_lock lock(mu_);
  if (!cv_.wait_for(lock, timeout, [&] { return !queue_.empty(); })) return std::nullopt;
  std::string out = std::move(queue_.front());
  queue_.pop_front();
  return out;
}

void NatsBus::Drain() {
  if (connected_) natsConnection_FlushTimeout(conn_, 1000);
  if (sub_ != nullptr) natsSubscription_Unsubscribe(sub_);
}

void NatsBus::OnMessage(natsConnection* /*nc*/, natsSubscription* /*sub*/, natsMsg* msg, void* closure) {
  auto* self = static_cast<NatsBus*>(closure);
  std::string payload(natsMsg_GetData(msg), static_cast<std::size_t>(natsMsg_GetDataLength(msg)));
  natsMsg_Destroy(msg);
  {
    const std::scoped_lock lock(self->mu_);
    if (self->queue_.size() >= kMaxQueued) {
      self->queue_.pop_front();
      ++self->dropped_inbound_;
    }
    self->queue_.push_back(std::move(payload));
  }
  self->cv_.notify_one();
}

void NatsBus::OnConnected(natsConnection* /*nc*/, void* closure) { static_cast<NatsBus*>(closure)->connected_ = true; }

void NatsBus::OnDisconnected(natsConnection* /*nc*/, void* closure) {
  static_cast<NatsBus*>(closure)->connected_ = false;
}

}  // namespace susq::needle
