// Minimal work-sharing loop over independent blocks.
#pragma once

#include <algorithm>
#include <atomic>
#include <cstddef>
#include <exception>
#include <mutex>
#include <thread>
#include <vector>

namespace susq::sim {

/// Resolves a requested thread count (0 = hardware concurrency), never more than `work_items`.
inline unsigned ResolveThreads(unsigned requested, std::size_t work_items) {
  unsigned n = requested == 0 ? std::max(1U, std::thread::hardware_concurrency()) : requested;
  if (work_items < n) n = static_cast<unsigned>(std::max<std::size_t>(1, work_items));
  return n;
}

/// Calls fn(i) for every i in [0, n_items) across `threads` workers. Items are claimed
/// dynamically, so fn must write only to item-owned output (results are then independent of
/// scheduling). The first exception thrown by any worker is rethrown on the caller.
template <class Fn>
void ParallelFor(std::size_t n_items, unsigned threads, Fn&& fn) {
  const unsigned n_threads = ResolveThreads(threads, n_items);
  if (n_threads <= 1) {
    for (std::size_t i = 0; i < n_items; ++i) fn(i);
    return;
  }
  std::atomic<std::size_t> next{0};
  std::exception_ptr error;
  std::mutex error_mu;
  {
    std::vector<std::jthread> workers;
    workers.reserve(n_threads);
    for (unsigned t = 0; t < n_threads; ++t) {
      workers.emplace_back([&] {
        try {
          for (std::size_t i = next.fetch_add(1); i < n_items; i = next.fetch_add(1)) fn(i);
        } catch (...) {
          const std::scoped_lock lock(error_mu);
          if (!error) error = std::current_exception();
          next.store(n_items);
        }
      });
    }
  }  // jthreads join here
  if (error) std::rethrow_exception(error);
}

}  // namespace susq::sim
