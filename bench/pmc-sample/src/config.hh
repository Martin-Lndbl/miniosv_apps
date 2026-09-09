#pragma once

// Compile-time knobs for the pmc-sample benchmark. Baked into the image and
// printed in the banner; override on the make command line:
//
//   just build apps/bench/pmc-sample -j8 PMC_REPS=9

#ifndef PMC_REPS
#define PMC_REPS 5
#endif

// Wall-clock budget per measurement, and the iteration cap that lets a fast
// operation reach it. 2s puts every repetition inside the 1-10s band: long
// enough that scheduler noise, a stray interrupt or a clock step are diluted
// rather than dominant, short enough that a sweep still fits the deploy
// deadline.
//
// The cap has to rise with the budget. loop_overhead runs at ~0.3ns, so at
// the old 5M ceiling it finished in 1.5ms no matter how large the budget was
// -- the clamp, not the budget, decided the duration.
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

// Hz. 4000 is `perf record`'s default; 997 and 99 are the primes people pick
// to avoid beating against a periodic workload.
#ifndef PMC_FREQS
#define PMC_FREQS                                                             \
  { 99, 997, 4000, 10000, 50000 }
#endif

// LCG iterations per unit (~5 cycles each). Small, so the loop is a smooth
// stream of work rather than a sequence of regions.
#ifndef PMC_WORK
#define PMC_WORK 64
#endif

// Ring of saved PCs. Power of two so the handler masks rather than divides.
#ifndef PMC_RING_BITS
#define PMC_RING_BITS 12
#endif

#ifndef PMC_CALIBRATE_TRIES
#define PMC_CALIBRATE_TRIES 5
#endif

#ifndef PMC_CALIBRATE_MS
#define PMC_CALIBRATE_MS 50
#endif

// Off by default: powering off takes the cloud serial console with it.
// `capture` passes 1 for a local run that terminates by itself.
#ifndef PMC_POWEROFF
#define PMC_POWEROFF 0
#endif
