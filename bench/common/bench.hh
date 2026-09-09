#pragma once

// Timing harness shared by the pmc-* benchmarks. Include the app's own
// config.hh first: the PMC_* sizing knobs come from there.

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <vector>

#include <osv/perf.hh>
#include <osv/power.hh>

// Defaulted so the header is self-contained; each app's config.hh normally
// sets it before this is included.
#ifndef PMC_POWEROFF
#define PMC_POWEROFF 0
#endif

namespace bench {

// Ends the run. Off by default, as in app/app.cc: powering off stops the
// machine the moment the run finishes and takes the cloud serial console
// output with it, so a deployed run has to stay up. `capture` passes
// PMC_POWEROFF=1 for a local run that terminates by itself.
[[noreturn]] inline void finish() {
#if PMC_POWEROFF
  osv::poweroff();
#else
  // The empty asm stops the compiler optimising the loop away.
  while (true) {
    asm volatile("" ::: "memory");
  }
#endif
}

inline volatile uint64_t sink;

// Not processor::ticks(): that is a plain (non-volatile) asm on x86, so the
// compiler CSEs the two reads bracketing a loop into one and every delta comes
// out zero. A memory barrier does not help -- rdtsc touches no memory.
inline uint64_t now_ticks() {
#if defined(__x86_64__)
  uint32_t lo, hi;
  asm volatile("rdtsc" : "=a"(lo), "=d"(hi));
  return lo | (static_cast<uint64_t>(hi) << 32);
#else
  uint64_t v;
  asm volatile("isb; mrs %0, cntvct_el0" : "=r"(v));
  return v;
#endif
}

struct Cost {
  double ns;
  double ticks; // TSC on x86, CNTVCT on aarch64
  uint64_t iters;
};

template <typename F> Cost time_loop(uint64_t iters, F &body) {
  auto t0 = std::chrono::steady_clock::now();
  uint64_t c0 = now_ticks();
  for (uint64_t i = 0; i < iters; ++i)
    body(i);
  uint64_t c1 = now_ticks();
  auto t1 = std::chrono::steady_clock::now();
  double ns = std::chrono::duration<double, std::nano>(t1 - t0).count();
  return {ns / static_cast<double>(iters),
          static_cast<double>(c1 - c0) / static_cast<double>(iters), iters};
}

// Probe, then size the real loop to a time budget. A fixed iteration count
// right for bare metal is unusable in a guest whose MSR accesses trap.
// Sized in two stages, because one is not enough. The probe is only
// PMC_PROBE_ITERS long, so for sub-nanosecond work it measures the two
// steady_clock reads more than the body: measured, a 0.28ns loop probed as
// ~1.7ns and the "2s" run came out at 0.34s. A cold first call is worse --
// pmc_start_with_conf's first repetition sized itself to 2671 iterations
// against 1.7M for the rest, a 3ms run inside a 2s budget.
//
// So: size from the probe, run, then re-size from that run and run again. The
// second estimate comes from a loop that actually lasted long enough to be
// accurate, which is what puts every repetition in the intended band.
template <typename F> Cost measure(F body) {
  auto sized = [](double per) {
    return std::clamp<uint64_t>(static_cast<uint64_t>(PMC_BUDGET_NS / per),
                                PMC_MIN_ITERS, PMC_MAX_ITERS);
  };
  Cost probe = time_loop(PMC_PROBE_ITERS, body);
  Cost first = time_loop(sized(probe.ns > 0 ? probe.ns : 1.0), body);
  return time_loop(sized(first.ns > 0 ? first.ns : 1.0), body);
}

inline double median(std::vector<double> v) {
  if (v.empty())
    return 0;
  std::sort(v.begin(), v.end());
  size_t n = v.size();
  return n % 2 ? v[n / 2] : 0.5 * (v[n / 2 - 1] + v[n / 2]);
}

inline void busy_wait_ms(int ms) {
  auto t0 = std::chrono::steady_clock::now();
  while (std::chrono::steady_clock::now() - t0 < std::chrono::milliseconds(ms))
    asm volatile("" ::: "memory");
}

inline double tick_hz() {
  auto t0 = std::chrono::steady_clock::now();
  uint64_t c0 = now_ticks();
  busy_wait_ms(PMC_CALIBRATE_MS);
  uint64_t c1 = now_ticks();
  auto t1 = std::chrono::steady_clock::now();
  double s = std::chrono::duration<double>(t1 - t0).count();
  return s > 0 ? static_cast<double>(c1 - c0) / s : 0;
}

// Core clock, from the PMU's own cycle counter. Not derivable from tick_hz:
// on aarch64 cntvct_el0 runs at the system counter frequency, unrelated to the
// core clock. Call before taking a counter for anything else.
//
// Retried and sanity-checked: measured across boots this returned 0 (the
// counter never advanced) or ~2^48 (Event::report underflowing when after <
// before) in roughly a third of runs, and cycles_per_op is derived from it.
inline double cpu_hz() {
  for (int attempt = 0; attempt < PMC_CALIBRATE_TRIES; ++attempt) {
    perf::PerfEvent e(false);
    e.registerCounter(perf::PERF_COUNT_HW::CPU_CYCLES);
    e.startCounters();
    busy_wait_ms(PMC_CALIBRATE_MS);
    e.stopCounters();
    double s = e.getDuration();
    double cyc = e.getCounter(perf::PERF_COUNT_HW::CPU_CYCLES.name);
    if (s <= 0 || cyc <= 0)
      continue;
    double hz = cyc / s;
    // No core here runs below 500 MHz or above 10 GHz; anything outside that
    // is a failed read, not a clock.
    if (hz > 5e8 && hz < 1e10)
      return hz;
  }
  return 0;
}

// Dependent LCG chain: linear in `work`, and free of cache and branch-predictor
// variance that would land on top of the overhead being measured.
inline uint64_t spin(uint64_t work, uint64_t x) {
  for (uint64_t i = 0; i < work; ++i) {
    x = x * 6364136223846793005ull + 1442695040888963407ull;
    asm volatile("" : "+r"(x));
  }
  return x;
}

// For the banner, so a log says what produced it.
inline const char *arch_name() {
#if defined(__x86_64__)
  return "x86_64";
#else
  return "aarch64";
#endif
}

inline const char *vendor_name() {
#if defined(__x86_64__)
  return perf::is_intel() ? "intel" : (perf::is_amd() ? "amd" : "unknown");
#else
  return "arm";
#endif
}

} // namespace bench
