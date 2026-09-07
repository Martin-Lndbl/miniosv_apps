#pragma once

// Compile-time knobs. Baked into the image and printed in the banner; override
// on the make command line, which `just build` forwards:
//
//   just build apps/bench/pmc-cost -j8 PMC_REPS=9

// Median is reported, so keep this odd.
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

// Unused here -- nothing calls cpu_hz() any more -- but bench.hh is
// header-only, so its definition still has to compile.
#ifndef PMC_CALIBRATE_TRIES
#define PMC_CALIBRATE_TRIES 5
#endif

#ifndef PMC_CALIBRATE_MS
#define PMC_CALIBRATE_MS 50
#endif

// Off by default, so the benchmark ends in an endless loop like app/app.cc:
// powering off stops the machine the moment the run finishes and takes the
// (cloud) serial console output with it. `capture` passes 1 to get a local run
// that terminates by itself; a deployed run wants the default.
#ifndef PMC_POWEROFF
#define PMC_POWEROFF 0
#endif
