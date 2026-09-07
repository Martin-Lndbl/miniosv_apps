// What each counting primitive costs, on its own.
//
// One steady_clock read, N identical calls on the same counter, one read
// after, divided by N. N comes from a wall-clock budget rather than a fixed
// count: under a hypervisor every PMU register access traps, so a count right
// for bare metal is two orders of magnitude wrong here.

#include <cstdint>
#include <cstdio>

#include <osv/perf.hh>

#include "config.hh"

#include "bench.hh"

extern "C" void osv_app_main() {
  perf::enable_pmu();
  perf::PMCSelectCore pmcs{perf::make_default_core_pmcs()};

  // Before anything can fail: on a virtualised instance the PMU may not be
  // exposed at all, and a run that dies then still has to say why. AWS slices
  // the count per VM -- c7i grants 8, c7a 5, both Gravitons 2.
  printf("pmc-cost: arch=%s vendor=%s pmu_counters=%u granted=%u reps=%d "
         "budget_ms=%.0f\n",
         bench::arch_name(), bench::vendor_name(), perf::pmu_num_counters(),
         static_cast<uint32_t>(pmcs.size_of_x(perf::PMClass::CORE)), PMC_REPS,
         PMC_BUDGET_NS / 1e6);

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

  row("pmc_start_with_conf",
      [&](uint64_t) { perf::pmc_start_with_conf(ctr, sel, bitmap); });
  row("pmc_read", [&](uint64_t) { acc += perf::pmc_read(ctr); });
  row("pmc_write", [&](uint64_t i) { perf::pmc_write_counter(ctr, i); });
  row("pmc_stop", [&](uint64_t) { perf::pmc_stop(sel); });

  bench::sink = acc;
  pmcs.release(pmc);

  printf("pmc-cost: done\n");
  bench::finish();
}
