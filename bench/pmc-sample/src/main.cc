// What sampling costs inside miniOSv.
//
// The overflow handler does the least a profiler can do -- save the
// interrupted PC -- so this measures the mechanism, not a profiler built on
// it. Overhead is against the same workload unsampled in the same boot; that
// ratio is what compares against competitors/linux-sample.
//
// 4000 Hz is `perf record`'s default. The PMU takes a period in events, so
// each frequency becomes cpu_hz/F cycles.

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <vector>

#include <osv/perf.hh>

#include "config.hh"

#include "bench.hh"

namespace {

constexpr size_t ring_size = 1u << PMC_RING_BITS;

// Volatile, not atomic: the handler runs on the sampled core with interrupts
// off, so an atomic RMW here would measure the atomic, not the sampling.
volatile uint64_t sample_count;
uintptr_t sample_pc[ring_size];

// The whole handler. A profiler would unwind from here; this is the floor.
void on_overflow(exception_frame *ef) {
  uint64_t n = sample_count;
  sample_pc[n & (ring_size - 1)] = reinterpret_cast<uintptr_t>(ef->get_pc());
  sample_count = n + 1;
}

// Sized once, then every loop below runs exactly this many units.
//
// Not bench::measure(): that sizes itself in three timed loops and reports only
// the last, so a sampler armed around the call collects overflows from all
// three while the duration it returns covers one. delivered_pct then reads
// ~200% -- measured on c7i.large as 196 samples/s against a requested 99 Hz. A
// fixed count makes the sampled window and the timed window the same window,
// and it is the rule competitors/linux-sample already uses, so the two sides
// compute the column the same way and spend equal wall time.
//
// Single-stage sizing is accurate here where it would not be in pmc-cost: one
// unit is PMC_WORK dependent LCG steps, ~70ns, so a PMC_PROBE_ITERS probe
// measures the work rather than the two clock reads bracketing it.
uint64_t sized_iters() {
  uint64_t x = 1;
  auto body = [&](uint64_t) { x = bench::spin(PMC_WORK, x); };
  bench::Cost probe = bench::time_loop(PMC_PROBE_ITERS, body);
  bench::sink = x;
  double per = probe.ns > 0 ? probe.ns : 1.0;
  return std::clamp<uint64_t>(static_cast<uint64_t>(PMC_BUDGET_NS / per),
                              PMC_MIN_ITERS, PMC_MAX_ITERS);
}

// ns per unit of work, over `iters` units. period == 0 is the unsampled
// baseline.
double workload_ns(uint64_t period, uint64_t iters, uint64_t &samples) {
  uint64_t x = 1;
  auto body = [&](uint64_t) { x = bench::spin(PMC_WORK, x); };

  if (period == 0) {
    bench::Cost c = bench::time_loop(iters, body);
    bench::sink = x;
    samples = 0;
    return c.ns;
  }

  perf::PMCSampler sampler(period, on_overflow);
  if (!sampler.start()) {
    samples = 0;
    return 0;
  }
  // Once, right after the first arm. On aarch64 the overflow interrupt can
  // fail three indistinguishable ways -- registered on an id the GIC never
  // raises, never armed, or armed on a counter that is not running -- and the
  // only visible symptom is a flood of unhandled irq=23. These are the three
  // registers that tell them apart, read back from the hardware rather than
  // assumed from what we wrote.
  static bool once = false;
  if (!once) {
    once = true;
    perf::PMCIntDebug d = perf::pmc_int_debug();
    printf("pmc-sample: irq_id=%u intenset=0x%llx ovsclr=0x%llx "
           "cntenset=0x%llx ctr=%u\n",
           d.irq_id, (unsigned long long)d.intenset,
           (unsigned long long)d.ovsclr, (unsigned long long)d.cntenset,
           sampler.counter_id());
  }
  // Zeroed here rather than before start(): the debug printf above goes to the
  // serial console, which is slow enough at 50 kHz to contribute thousands of
  // overflows to a window it is not part of.
  sample_count = 0;
  bench::Cost c = bench::time_loop(iters, body);
  sampler.stop();

  bench::sink = x;
  samples = sample_count;
  return c.ns;
}

} // namespace

extern "C" void osv_app_main() {
  // Stage markers. A cloud run that produces nothing is indistinguishable
  // from one that never booted, and on c7g.large this benchmark produced no
  // console output at all while pmc-cost on the same image path was fine.
  // The last line printed says how far it got.
  printf("pmc-sample: start arch=%s\n", bench::arch_name());

  perf::enable_pmu();
  printf("pmc-sample: pmu enabled\n");

  double hz = bench::cpu_hz();
  printf("pmc-sample: cpu_mhz=%.1f\n", hz / 1e6);

  perf::PMCSelectCore probe{perf::make_default_core_pmcs()};
  uint32_t granted =
      static_cast<uint32_t>(probe.size_of_x(perf::PMClass::CORE));
  printf("pmc-sample: granted=%u\n", granted);

  printf("pmc-sample: arch=%s vendor=%s pmu_counters=%u granted=%u "
         "cpu_mhz=%.1f work=%d reps=%d budget_ms=%.0f\n",
         bench::arch_name(), bench::vendor_name(), perf::pmu_num_counters(),
         granted, hz / 1e6, PMC_WORK, PMC_REPS, PMC_BUDGET_NS / 1e6);

  if (granted == 0 || hz <= 0) {
    printf("pmc-sample: no usable PMU here (granted=%u cpu_mhz=%.1f)\n",
           granted, hz / 1e6);
    bench::finish();
  }

  uint64_t iters = sized_iters();
  printf("pmc-sample: iters=%llu\n", (unsigned long long)iters);

  // The same measurement twice, unsampled: an overhead below this is not
  // evidence of an overhead.
  std::vector<double> control;
  for (int r = 0; r < PMC_REPS; ++r) {
    uint64_t s = 0;
    double a = workload_ns(0, iters, s);
    double b = workload_ns(0, iters, s);
    control.push_back(a > 0 ? 100.0 * (b - a) / a : 0.0);
    if (control.back() < 0)
      control.back() = -control.back();
  }
  printf("pmc-sample: noise_floor_pct=%.3f\n",
         bench::median(std::move(control)));

  // One row per repetition, not a median: dispersion has to survive into the
  // CSV, and a single boot's median hides the spread between boots.
  //
  // delivered_pct is the validity gate: a sampler that stops firing part-way
  // looks cheap, and without this column that reads as a good result. `dead`
  // marks a rep that armed but never fired -- broken, not cheap.
  printf("#SAMPLE,rep,freq_hz,period_events,base_ns,sampled_ns,samples,"
         "samples_per_s,delivered_pct,dead\n");

  static const int freqs[] = PMC_FREQS;

  for (int f : freqs) {
    uint64_t period = static_cast<uint64_t>(hz / f);
    for (int r = 0; r < PMC_REPS; ++r) {
      uint64_t samples = 0;
      // Interleaved so drift over the run lands on both arms, and over the same
      // iteration count so the two are a like-for-like pair.
      double b = workload_ns(0, iters, samples);
      double s = workload_ns(period, iters, samples);
      int dead = (s <= 0 || samples == 0);
      double secs = dead ? 0 : s * iters / 1e9;
      double sps = secs > 0 ? samples / secs : 0;
      printf("SAMPLE,%d,%d,%llu,%.4f,%.4f,%llu,%.1f,%.1f,%d\n", r, f,
             static_cast<unsigned long long>(period), b, s,
             static_cast<unsigned long long>(samples), sps, 100.0 * sps / f,
             dead);
    }
  }

  printf("pmc-sample: done\n");
  bench::finish();
}
