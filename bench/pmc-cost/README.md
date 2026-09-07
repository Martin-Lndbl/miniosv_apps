# pmc-cost

What performance monitoring costs inside miniOSv.

```sh
just setup apps/bench/pmc-cost      # check this host can produce a real run

# local: build, run, split the CSVs into results/pmc-cost/
just --justfile apps/bench/pmc-cost/justfile capture --nogdb

# EC2: build, deploy, stream the console into results/pmc-cost/
just --justfile apps/bench/pmc-cost/justfile deploy c7i.large
```

Like `app/app.cc`, the benchmark ends in an endless loop rather than powering
off, so a deployed run keeps its console output instead of the instance
stopping the moment it finishes. `capture` overrides that with
`PMC_POWEROFF=1`, because a local run has to return; a plain `just run` does
not end by itself (Ctrl-A X quits QEMU).

`deploy` streams until you Ctrl+C, which is also what terminates the instance
and deletes the AMI and snapshot — so wait for `pmc-cost: done`.

## What it measures

Two parts, one binary, both printed as CSV on the serial console.

**`PRIM`** — the cost of each counting primitive on its own, amortised over a
sized loop. No workload, no attribution: just the instruction sequence the shim
issues. `loop_overhead` and `read_tsc` are there as the floor and the
measurement cost, so the interesting rows can be read against something.
`loop_overhead` landing at ~1 cycle/op is the harness self-check — if it drifts,
the optimiser got into the timing loop.

**`REGION`** — the cost of instrumenting real work, swept over how much work
sits inside one instrumented region (`work` is LCG iterations, ~5 cycles each).
This is the part that answers *can I afford to measure this?*, and it produces a
curve rather than a number: instrumentation is free at millisecond granularity
and ruinous at nanosecond granularity, and the result is where it crosses.

Each point is run at 1 counter and at 4 (what `PerfEvent` registers by default),
which separates the fixed cost of a start/stop pair from the per-counter cost.

## Reading the numbers

Overhead is always computed against an uninstrumented run of *the same workload,
in the same boot, on the same machine*. That ratio is the only quantity
comparable against Linux. Absolute runtimes would compare the two operating
systems, which is a different experiment — and one that would flatter or damn
this work for reasons that have nothing to do with monitoring.

So the comparison against Linux is **ratio to ratio**: the Linux side runs the
identical sweep over `perf_event_open`, computes its own overhead against its
own baseline, and the two curves go on one axis.

## Where you run it decides what it means

Under KVM every PMU MSR access is a VM exit. On this host that puts `pmc_read`
at ~4600 cycles, where bare metal would be ~100. Everything downstream inherits
that factor, so a local run **understates the design by roughly an order of
magnitude** and is a smoke test, not a result.

Run the real thing on bare metal or a `.metal` instance. The banner prints the
tick rate, the vendor and how many counters the PMU actually granted (AWS slices
this per VM), so a log can be checked for what it was measured on.

One consequence worth knowing before reading a `PRIM` table: `pmc_read` is
`rdmsr`, and `rdmsr` is markedly slower than `rdpmc` even on bare metal. Linux's
self-monitoring path uses `rdpmc` through the mmap'd page, so on the raw
read-cost row Linux may well win. The counting story's strength is the absence
of setup, file descriptors and syscalls — not the per-read cost. Worth measuring
early, because if it matters the fix is to put `pmc_read` on `rdpmc`.

## Knobs

Compile-time, so a value is baked into the image and printed in the banner.
Defaults are in `src/config.hh`; the Makefile forwards these as `-D`:

```sh
just build apps/bench/pmc-cost -j8 PMC_REPS=9 PMC_COUNTERS=2 PMC_POWEROFF=0
```

`PMC_REPS` (median of), `PMC_BUDGET_NS`, `PMC_PROBE_ITERS`, `PMC_MIN_ITERS`,
`PMC_MAX_ITERS`, `PMC_COUNTERS`, `PMC_CALIBRATE_MS`, `PMC_POWEROFF`.

`PMC_WORK_STEPS` is a braced list and does not survive a shell round trip —
edit `config.hh` to change the sweep.

Every loop is sized from a probe to `PMC_BUDGET_NS` rather than run for a fixed
iteration count. That is what lets one binary be sane both on bare metal and in
a guest whose MSR accesses trap; a fixed count right for one is unusable on the
other.

`PMC_POWEROFF` defaults to 0 (endless loop). Set it to 1 only for a run that
must return by itself, as `capture` does.

## Not here yet

The Linux arm (`competitors/`): the same sweep over `perf_event_open`, reading
counters three ways — `read()` on the fd, `rdpmc` via the mmap'd page, and
`PerfEvent.h` as the thing people actually use. Until that exists this bench
produces one curve, not a comparison.
