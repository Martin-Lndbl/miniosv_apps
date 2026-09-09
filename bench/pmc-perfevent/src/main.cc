// What miniOSv's PerfEvent costs, against Viktor Leis's PerfEvent.hpp on Linux
// running the identical hashtable workload and the identical event set.
//
// Only startCounters/stopCounters are inside the timed region on both sides;
// constructing the header is not. That is the generous reading for Linux,
// where construction is seven perf_event_open syscalls.

#include <cstdint>
#include <cstdio>
#include <vector>

#include <osv/perf.hh>
#include <osv/sched.hh>

#include "config.hh"

#include "bench.hh"
#include "hashtable.hh"

namespace {

double now_ns() {
  return std::chrono::duration<double, std::nano>(
             std::chrono::steady_clock::now().time_since_epoch())
      .count();
}

// Two general-purpose events, matched one-for-one with the Linux side.
//
// Not CPU_CYCLES: it is PMClass::CYCLES, so on aarch64 it belongs to the
// dedicated PMCCNTR_EL0 and on x86 to a fixed-function counter -- measuring
// it does not exercise a general counter, and it does not consume one of the
// two the hypervisor grants on Graviton. Not the cache-miss event either: it
// is the most microarchitecture-dependent of the four, so it is the weakest
// thing to hold constant across three vendors.
void register_n(perf::PerfEvent &e, int n) {
  using namespace perf::PERF_COUNT_HW;
  if (n > 0) e.registerCounter(INSTRUCTIONS);
  if (n > 1) e.registerCounter(BRANCH_MISS);
}

} // namespace

extern "C" void osv_app_main() {
  // A PMC belongs to a core, so PMCSelect::acquire() refuses a thread that
  // could migrate. Any cpu will do; what matters is that it stops changing.
  sched::thread::pin(sched::cpu::current());

  perf::enable_pmu();

  perf::PMCSelectCore probe{perf::make_default_core_pmcs()};
  uint32_t granted =
      static_cast<uint32_t>(probe.size_of_x(perf::PMClass::CORE));
  int events = PMC_EVENTS < static_cast<int>(granted)
                   ? PMC_EVENTS
                   : static_cast<int>(granted);

  // First touch of the 16 MiB table, so the first timed region is not the only
  // one paying for it.
  ht::clear();

  printf("pmc-perfevent: os=miniosv arch=%s vendor=%s granted=%u events=%d "
         "reps=%d ht_mib=%u\n",
         bench::arch_name(), bench::vendor_name(), granted, events, PMC_REPS,
         static_cast<unsigned>((ht::size * sizeof(uint64_t)) >> 20));
  // One row per repetition, not a median: dispersion has to survive into the
  // CSV or the plot cannot show it, and a single boot's median hides both the
  // jitter within a boot and the ~14% clock spread between boots.
  printf("#PERFEVENT,rep,keys,base_ns,instr_ns,delta_ns,mult_pct,events\n");

  static const size_t works[] = PMC_WORKS;

  for (size_t w : works) {
    for (int r = 0; r < PMC_REPS; ++r) {
      // Interleaved so drift over the run lands on both arms.
      ht::clear();
      double t0 = now_ns();
      bench::sink = ht::work(w, 1 + r);
      double t1 = now_ns();

      perf::PerfEvent e(false);
      register_n(e, events);
      ht::clear();
      double t2 = now_ns();
      e.startCounters();
      bench::sink = ht::work(w, 1 + r);
      e.stopCounters();
      double t3 = now_ns();

      // mult_pct is 100 by construction: miniOSv never multiplexes -- it
      // either gets the counters or refuses. Printed so the schemas match.
      printf("PERFEVENT,%d,%llu,%.0f,%.0f,%.0f,100.0,%d\n", r,
             static_cast<unsigned long long>(w), t1 - t0, t3 - t2,
             (t3 - t2) - (t1 - t0), events);
    }
  }

  printf("pmc-perfevent: done\n");
  bench::finish();
}
