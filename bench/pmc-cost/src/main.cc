// What each counting primitive costs, on its own.
//
// One steady_clock read, N identical calls on the same counter, one read
// after, divided by N. N comes from a wall-clock budget rather than a fixed
// count: under a hypervisor every PMU register access traps, so a count right
// for bare metal is two orders of magnitude wrong here.

#include <cstdint>
#include <cstdio>

#include <osv/perf.hh>
#include <osv/sched.hh>

#include "config.hh"

#include "bench.hh"

extern "C" void osv_app_main() {
  // A PMC belongs to a core, so PMCSelect::acquire() refuses a thread that
  // could migrate. Any cpu will do; what matters is that it stops changing.
  sched::thread::pin(sched::cpu::current());

  perf::enable_pmu();
  perf::PMCSelectCore pmcs{perf::make_default_core_pmcs()};

  // The core clock, measured from the PMU's own cycle counter rather than
  // assumed from the instance type. Nanoseconds alone are not comparable
  // between a guest and bare metal: nothing manages P-states on metal, so the
  // core can sit far below its rated clock, and a wall-clock figure then says
  // as much about the frequency as about the operation. Recorded here so the
  // plot can divide it out; on c5.metal an empty loop measured 2.00ns against
  // 0.28ns in a guest, which is the symptom this exists to explain.
  double hz = bench::cpu_hz();

  // Before anything can fail: on a virtualised instance the PMU may not be
  // exposed at all, and a run that dies then still has to say why. AWS slices
  // the count per VM -- c7i grants 8, c7a 5, both Gravitons 2.
  printf("pmc-cost: arch=%s vendor=%s pmu_counters=%u granted=%u reps=%d "
         "budget_ms=%.0f cpu_mhz=%.1f\n",
         bench::arch_name(), bench::vendor_name(), perf::pmu_num_counters(),
         static_cast<uint32_t>(pmcs.size_of_x(perf::PMClass::CORE)), PMC_REPS,
         PMC_BUDGET_NS / 1e6, hz / 1e6);

  perf::PMC *pmc = pmcs.acquire(perf::PMClass::CORE);
  if (!pmc) {
    printf("pmc-cost: no core counter available -- no vPMU here\n");
    bench::finish();
  }

  const uint32_t ctr = pmc->perfCtr;
  const uint32_t sel = pmc->perfEvtSel;
  // A general-purpose event, matching the general-purpose counter acquired
  // above. CPU_CYCLES is PMClass::CYCLES: on aarch64 it belongs to the
  // dedicated PMCCNTR_EL0 and on x86 to a fixed-function counter, so
  // programming it into a general counter's event select measures a pairing
  // the interface would not normally make.
  const uint64_t bitmap = perf::PERF_COUNT_HW::BRANCH_MISS.bitmap;
  uint64_t acc = 0;

  // One row per repetition, not a median: a single boot's median hides the
  // spread between boots.
  printf("#PRIM,rep,op,iters,ns_per_op\n");

  auto row = [&](const char *op, auto body) {
    for (int r = 0; r < PMC_REPS; ++r) {
      bench::Cost c = bench::measure(body);
      printf("PRIM,%d,%s,%llu,%.2f\n", r, op,
             static_cast<unsigned long long>(c.iters), c.ns);
    }
  };

  // The floor the others sit on. The empty asm is a barrier, not an
  // implementation -- without it the loop folds away and the floor reads zero.
  row("loop_overhead", [&](uint64_t) { asm volatile("" : "+r"(acc)); });

  const perf::PMClass cls = pmc->pmClass;
  row("pmc_start_with_conf",
      [&](uint64_t) { perf::pmc_start_with_conf(ctr, sel, cls, bitmap); });
  row("pmc_read", [&](uint64_t) { acc += perf::pmc_read(ctr, cls); });
  row("pmc_write", [&](uint64_t i) { perf::pmc_write_counter(ctr, cls, i); });
  row("pmc_stop", [&](uint64_t) { perf::pmc_stop(sel, cls); });

  bench::sink = acc;
  pmcs.release(pmc);

  printf("pmc-cost: done\n");
  bench::finish();
}
