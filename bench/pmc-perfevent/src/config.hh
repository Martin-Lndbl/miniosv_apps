#pragma once

// Knobs for pmc-perfevent. Override on the make command line:
//   just build apps/bench/pmc-perfevent -j8 PMC_EVENTS=2

#ifndef PMC_REPS
#define PMC_REPS 5
#endif

// How many of PerfEvent's default counters to register. Matched against the
// Linux side so the two compare headers, not event counts.
//
// Two, not four, and the same on every machine. Nitro grants a guest only 2
// counters on Graviton -- measured on both c7g.large and c8g.large, so it is
// hypervisor policy rather than a generation limit -- while x86 gets 8 on c7i
// and 5 on c7a. Letting each machine use what it was granted made the bars
// incomparable across vendors: Graviton was measuring half the work of the
// others. Pinning every target to the lowest common count costs some absolute
// magnitude and buys a figure that means what it looks like it means.
#ifndef PMC_EVENTS
#define PMC_EVENTS 2
#endif

// Keys inserted and probed per region, same list as the Linux side.
// Kept small: the added cost is tens of microseconds, so a region of
// milliseconds buries it under the workload's own run-to-run jitter.
#ifndef PMC_WORKS
#define PMC_WORKS                                                             \
  { 500, 2000, 8000, 32000, 128000 }
#endif

// bench.hh's tick_hz/cpu_hz are plain inline functions, so these have to be
// defined even though this bench does not size loops by a budget.
#ifndef PMC_CALIBRATE_TRIES
#define PMC_CALIBRATE_TRIES 5
#endif

#ifndef PMC_CALIBRATE_MS
#define PMC_CALIBRATE_MS 50
#endif
#ifndef PMC_BUDGET_NS
#define PMC_BUDGET_NS 2000000000.0
#endif
#ifndef PMC_PROBE_ITERS
#define PMC_PROBE_ITERS 32
#endif
#ifndef PMC_MIN_ITERS
#define PMC_MIN_ITERS 32ull
#endif
#ifndef PMC_MAX_ITERS
#define PMC_MAX_ITERS 20000000000ull
#endif

#ifndef PMC_POWEROFF
#define PMC_POWEROFF 0
#endif
