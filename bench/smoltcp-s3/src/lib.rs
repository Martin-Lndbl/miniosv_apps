/*!
smoltcp-s3: how fast can miniOSv pull bytes out of S3?

W workers, each pinned to a CPU and owning one RSS queue, each driving M
parallel ranged GETs. Every connection fetches one fixed-size block, so the
bytes moved scale with worker count instead of being divided by it, and a
throughput-vs-parallelism curve stays a curve about parallelism.

The stack is `modules/mininet` in the miniOSv tree. Everything here is the
benchmark: what to fetch, how to split it up, and whether the run was complete.
Nothing below drives a NIC.
*/

#![no_std]

extern crate alloc;

mod config;
mod selftest;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use spin::Mutex;

use mininet::print::BufWriter;
use mininet::{
    println, thread, BodySink, Config, Endpoint, Stack, Step, Worker, WorkerConfig, WorkerHandle,
};

use config::{
    BLOCKS_PER_WORKER, BLOCK_SIZE, CONNS_PER_WORKER, N_WORKERS_REQ, OBJECT_SIZE, PLAIN_HTTP, RX_DESC,
    SYN_REDIAL_MS, TARGET_HOST, TARGET_IP, TARGET_PATH,
};

/// Counts the plaintext body and drops it. The benchmark measures the stack
/// up to delivery; what a caller does with the bytes is DuckDB's business.
#[derive(Default)]
struct CountSink(u64);

impl BodySink for CountSink {
    fn write(&mut self, data: &[u8]) {
        self.0 += data.len() as u64;
    }

    fn written(&self) -> u64 {
        self.0
    }
}

/// Test knob: abandon this many connections per worker on purpose. A
/// completeness check that has never been seen to fail is not a check, so
/// setting this to 1 must turn a passing run into an INCOMPLETE one.
const FAULT_ABANDON_CONNS: usize = 0;

/// What one worker thread reports back. Shared with the thread that spawned
/// it, so every field is atomic rather than the whole thing being locked.
#[derive(Default)]
struct WorkerResult {
    bytes_received: AtomicU64,
    elapsed_ns: AtomicU64,
    /// Wall time opening connections, before the first SYN could leave.
    dial_ns: AtomicU64,
    /// Time from the first slot running out of blocks to the last finishing.
    tail_ns: AtomicU64,
    /// Absolute instants (the shared clock): every slot dialled, and the first
    /// slot finding no block left. Between the last worker's `full` and the
    /// first worker's `idle` every slot on the machine is busy.
    full_abs_ns: AtomicU64,
    idle_abs_ns: AtomicU64,
    /// Connections dialled again because a SYN went unanswered.
    redials: AtomicU64,
    /// Completeness accounting: what this worker's connections were asked to
    /// fetch, and how many finished rather than being abandoned.
    bytes_expected: AtomicU64,
    conns_total: AtomicU64,
    conns_clean: AtomicU64,
    /// Non-206 responses, and the first such code. 0 means none: not a status.
    bad_status: AtomicU64,
    first_bad_status: AtomicU64,
    /// Responses whose parsed head did not describe the range that was asked
    /// for. Validates the header parser against what S3 actually sends.
    hdr_bad: AtomicU64,
}

fn build_range_request(buf: &mut [u8], start: u64, end_inclusive: u64) -> usize {
    let mut w = BufWriter::new(buf);
    let _ = write!(
        &mut w,
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: minidpdk-smoltcp/0.1\r\nRange: bytes={}-{}\r\nConnection: close\r\n\r\n",
        TARGET_PATH, TARGET_HOST, start, end_inclusive,
    );
    w.used()
}

/// Blocks are laid out consecutively and wrap within the object, so distinct
/// connections read distinct offsets rather than replaying one hot range.
fn block_range(block: u64) -> (u64, u64) {
    let stride = OBJECT_SIZE.saturating_sub(BLOCK_SIZE) + 1;
    let start = if stride == 0 {
        0
    } else {
        block.wrapping_mul(BLOCK_SIZE) % stride
    };
    (start, start + BLOCK_SIZE - 1)
}

/// Blocks a worker owns: `BLOCKS_PER_WORKER`, or one per connection.
fn blocks_per_worker() -> u64 {
    if BLOCKS_PER_WORKER == 0 {
        CONNS_PER_WORKER as u64
    } else {
        BLOCKS_PER_WORKER
    }
}

/// Open `slot` on the range of `block`. False if the socket refused, in which
/// case the range stays unrequested and the completeness check says so.
fn dial(w: &mut Worker, slot: usize, block: u64) -> bool {
    let (start, end) = block_range(block);
    let mut head = [0u8; 384];
    let n = build_range_request(&mut head, start, end);
    w.connect_next(slot, &head[..n], Box::new(CountSink::default())).is_ok()
}

/// One worker: keep every slot on a block until its blocks are gone.
fn run_worker(handle: WorkerHandle, peer: Endpoint, first_block: u64, out: Arc<WorkerResult>) {
    let queue_id = handle.queue_id();

    let mut cfg = WorkerConfig::new(peer);
    cfg.conns = CONNS_PER_WORKER;
    let mut w = match Worker::new(handle, &cfg) {
        Ok(w) => w,
        Err(e) => {
            println!("FAIL: q{}: {:?}", queue_id, e);
            return;
        }
    };

    let slots = w.slots();
    // Fault injection leaves the last few blocks unrequested -- exactly the
    // failure the completeness check has to catch.
    let blocks = blocks_per_worker().saturating_sub(FAULT_ABANDON_CONNS as u64);
    let mut next_block: u64 = 0;
    // What each slot is fetching, and when it dialled, for the head check
    // and the SYN re-dial.
    let mut asked: Vec<Option<(u64, u64)>> = alloc::vec![None; slots];
    let mut dialed_ns: Vec<u64> = alloc::vec![0; slots];
    let mut expected: u64 = 0;

    let mut bytes = 0u64;
    let mut total = 0u64;
    let mut clean = 0u64;
    let mut bad = 0u64;
    let mut first_bad = 0u64;
    let mut hdr_bad = 0u64;
    let mut redials = 0u64;

    // smoltcp holds every SYN until the `poll` after this loop, so time spent
    // here is time no handshake could start.
    let dial_start_ns = w.clock().elapsed_ns();
    for slot in 0..slots {
        if next_block >= blocks {
            break;
        }
        let block = first_block + next_block;
        next_block += 1;
        if dial(&mut w, slot, block) {
            let (start, end) = block_range(block);
            asked[slot] = Some((start, end));
            dialed_ns[slot] = w.clock().elapsed_ns();
            expected += end - start + 1;
        }
    }

    let start_ns = w.clock().elapsed_ns();
    let dial_ns = start_ns.saturating_sub(dial_start_ns);
    // From the first slot that found no block left to the end: time this
    // worker ran below its concurrency.
    let mut first_idle_ns: Option<u64> = None;
    loop {
        w.poll();
        let now_ns = w.clock().elapsed_ns();
        let mut busy = false;
        for slot in 0..slots {
            let (outcome, established) = match w.conn(slot) {
                Some(c) => (c.outcome(), c.established()),
                None => continue,
            };
            let Some(step) = outcome else {
                busy = true;
                if SYN_REDIAL_MS > 0
                    && !established
                    && now_ns.saturating_sub(dialed_ns[slot]) > SYN_REDIAL_MS * 1_000_000
                {
                    // Same range, fresh port: the range was already counted.
                    redials += 1;
                    w.release(slot);
                    let (s0, e0) = asked[slot].expect("a dialled slot has a range");
                    let mut head = [0u8; 384];
                    let n = build_range_request(&mut head, s0, e0);
                    if w.connect_next(slot, &head[..n], Box::new(CountSink::default())).is_ok() {
                        dialed_ns[slot] = now_ns;
                    }
                }
                continue;
            };
            // The block is done, one way or the other: account for it.
            let c = w.conn(slot).expect("just observed");
            bytes += c.sink_written();
            total += 1;
            if step == Step::Complete {
                clean += 1;
            } else {
                // A block that did not finish never delivered those bytes --
                // surface it here, not as a silent gap.
                println!(
                    "q{}: slot {} did not finish ({:?}, {} B)",
                    queue_id,
                    slot,
                    step,
                    c.sink_written()
                );
            }
            // The request is ranged, so only 206 is what we asked for. A status
            // of 0 means no head was ever parsed, so there is nothing to judge.
            if c.status() != 0 {
                if c.status() != 206 {
                    bad += 1;
                    if first_bad == 0 {
                        first_bad = c.status() as u64;
                    }
                }
                // Check the parsed head against what this slot actually
                // requested: a misparsed Content-Length is exactly the bug
                // that would later hand DuckDB a short page unnoticed.
                if let Some((start, end)) = asked[slot] {
                    let want = end - start + 1;
                    let head = c.head();
                    let len_ok = head.content_length == Some(want);
                    let range_ok = head
                        .content_range
                        .map(|r| r.first == start && r.last == end && r.total == Some(OBJECT_SIZE))
                        .unwrap_or(false);
                    // S3 always sends one on a 206; its absence is a parse
                    // failure, not a server that chose not to.
                    let etag_ok = head.etag.is_some();
                    if !len_ok || !range_ok || !etag_ok {
                        hdr_bad += 1;
                        if hdr_bad == 1 {
                            println!(
                                "q{}: head mismatch in slot {}: content-length {:?} (want {}), \
                                 content-range {:?} (want {}-{}/{}), etag {:?}",
                                queue_id,
                                slot,
                                head.content_length,
                                want,
                                head.content_range,
                                start,
                                end,
                                OBJECT_SIZE,
                                head.etag
                            );
                        }
                    }
                }
            }
            w.release(slot);
            asked[slot] = None;
            if next_block < blocks {
                let block = first_block + next_block;
                next_block += 1;
                if dial(&mut w, slot, block) {
                    let (start, end) = block_range(block);
                    asked[slot] = Some((start, end));
                    dialed_ns[slot] = now_ns;
                    expected += end - start + 1;
                    busy = true;
                }
            } else if first_idle_ns.is_none() {
                first_idle_ns = Some(now_ns);
            }
        }
        if !busy {
            break;
        }
    }
    let end_ns = w.clock().elapsed_ns();
    let elapsed_ns = end_ns.saturating_sub(start_ns);
    let tail_ns = end_ns.saturating_sub(first_idle_ns.unwrap_or(end_ns));
    let epoch = w.clock().epoch_ns();
    out.full_abs_ns.store(epoch + start_ns, Ordering::Relaxed);
    out.idle_abs_ns.store(epoch + first_idle_ns.unwrap_or(end_ns), Ordering::Relaxed);

    out.bytes_received.store(bytes, Ordering::Relaxed);
    out.elapsed_ns.store(elapsed_ns, Ordering::Relaxed);
    out.dial_ns.store(dial_ns, Ordering::Relaxed);
    out.tail_ns.store(tail_ns, Ordering::Relaxed);
    out.redials.store(redials, Ordering::Relaxed);
    out.bytes_expected.store(expected, Ordering::Relaxed);
    out.conns_total.store(total, Ordering::Relaxed);
    out.conns_clean.store(clean, Ordering::Relaxed);
    out.bad_status.store(bad, Ordering::Relaxed);
    out.first_bad_status.store(first_bad, Ordering::Relaxed);
    out.hdr_bad.store(hdr_bad, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub extern "C" fn osv_app_main() {
    if config::SELFTEST {
        selftest::run();
    }
    // tls_stub is always false now; the driver still reads the field.
    println!(
        "bench: {} workers x {} conns x {} MiB block, tls_stub=false scheme={} blocks={} rx_desc={} redial_ms={}",
        N_WORKERS_REQ,
        CONNS_PER_WORKER,
        BLOCK_SIZE / (1024 * 1024),
        if PLAIN_HTTP { "http" } else { "https" },
        blocks_per_worker(),
        RX_DESC,
        SYN_REDIAL_MS
    );

    let peer = Endpoint::new(TARGET_IP, TARGET_HOST, !PLAIN_HTTP);
    let t = peer.ip;
    println!(
        "target: {}.{}.{}.{}:{} {}",
        t[0], t[1], t[2], t[3], peer.port, TARGET_HOST
    );
    if OBJECT_SIZE == 0 || BLOCK_SIZE == 0 || CONNS_PER_WORKER == 0 || N_WORKERS_REQ == 0 {
        println!("FAIL: BENCH_WORKERS, BENCH_CONNS_PER_WORKER, BENCH_BLOCK_SIZE and AWS_BUCKET_SIZE must be nonzero");
        exit();
    }

    let stack = match Stack::up(&Config {
        queues: N_WORKERS_REQ,
        rx_desc: RX_DESC,
    }) {
        Ok(s) => s,
        Err(e) => {
            println!("FAIL: {:?}", e);
            exit();
        }
    };
    let n_workers = stack.queues();

    // No port slabs and no file split: each worker derives its own source
    // ports from the RSS hash, which partitions the range disjointly by
    // construction, and each connection fetches one whole block.
    let results: Vec<Arc<WorkerResult>> = (0..n_workers)
        .map(|_| Arc::new(WorkerResult::default()))
        .collect();

    let overall_clk = mininet::MonoClock::new();
    // The NIC's byte counter every 10 ms from a cpu no worker owns: one
    // clock, one counter, so the rate over any window is a measurement and
    // not a sum of per-worker rates. Spins between samples; the cpu is spare.
    let sampling = Arc::new(AtomicBool::new(true));
    let samples: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::with_capacity(8192)));
    let sampler = {
        let sampling = sampling.clone();
        let samples = samples.clone();
        let cpu = if n_workers as usize + 1 < cpu_count() { Some(n_workers as usize) } else { None };
        thread::spawn(
            move || {
                let clk = mininet::MonoClock::new();
                let mut next = 0u64;
                while sampling.load(Ordering::Relaxed) {
                    let t = clk.elapsed_ns();
                    if t >= next {
                        if let Some(n) = mininet::eth_stats() {
                            samples.lock().push((clk.epoch_ns() + t, n.ibytes));
                        }
                        next = t + 10_000_000;
                    }
                    core::hint::spin_loop();
                }
            },
            cpu,
        )
    };
    let threads: Vec<_> = (0..n_workers)
        .map(|i| {
            let handle = stack.handle(i).expect("queue granted by the device");
            let peer = peer.clone();
            let out = results[i as usize].clone();
            let first_block = (i as u64) * blocks_per_worker();
            thread::spawn(
                move || run_worker(handle, peer, first_block, out),
                Some(i as usize),
            )
        })
        .collect();
    for t in threads {
        t.join();
    }
    let overall_ns = overall_clk.elapsed_ns();
    sampling.store(false, Ordering::Relaxed);
    sampler.join();

    report(&results, n_workers, overall_ns);
    wire_windows(&results, &samples.lock(), overall_clk.epoch_ns());
    exit();
}

fn report(results: &[Arc<WorkerResult>], n_workers: u16, overall_ns: u64) {
    let mut total_b = 0u64;
    let mut total_expected = 0u64;
    let mut conns_total = 0u64;
    let mut conns_clean = 0u64;
    let mut bad = 0u64;
    let mut first_bad = 0u64;
    let mut hdr_bad = 0u64;
    let mut dial_ns_max = 0u64;
    let mut tail_ns_max = 0u64;
    let mut tail_ns_sum = 0u64;
    let mut redials = 0u64;
    // Each worker's rate while every slot of it was busy, summed: what the
    // stack sustains, as opposed to what the slowest connection leaves of it.
    let mut steady_bps = 0f64;

    for (i, r) in results.iter().enumerate() {
        let b = r.bytes_received.load(Ordering::Relaxed);
        let e = r.elapsed_ns.load(Ordering::Relaxed) as f64 / 1e9;
        dial_ns_max = dial_ns_max.max(r.dial_ns.load(Ordering::Relaxed));
        let tail = r.tail_ns.load(Ordering::Relaxed);
        tail_ns_max = tail_ns_max.max(tail);
        tail_ns_sum += tail;
        redials += r.redials.load(Ordering::Relaxed);
        let busy_s = (r.elapsed_ns.load(Ordering::Relaxed).saturating_sub(tail)) as f64 / 1e9;
        steady_bps += b as f64 * 8.0 / busy_s.max(1e-9);
        total_b += b;
        total_expected += r.bytes_expected.load(Ordering::Relaxed);
        conns_total += r.conns_total.load(Ordering::Relaxed);
        conns_clean += r.conns_clean.load(Ordering::Relaxed);
        bad += r.bad_status.load(Ordering::Relaxed);
        hdr_bad += r.hdr_bad.load(Ordering::Relaxed);
        if first_bad == 0 {
            first_bad = r.first_bad_status.load(Ordering::Relaxed);
        }
        println!(
            "worker {} (q{}): {} B / {:.3} s  ({:.1} MB/s)",
            i,
            i,
            b,
            e,
            (b as f64 / 1e6) / e.max(1e-9)
        );
    }

    let overall_s = overall_ns as f64 / 1e9;
    println!();
    println!(
        "AGGREGATE: {:.1} MiB in {:.3} s => {:.1} MB/s, {:.3} Gbps",
        total_b as f64 / (1024.0 * 1024.0),
        overall_s,
        total_b as f64 / 1e6 / overall_s.max(1e-9),
        total_b as f64 * 8.0 / 1e9 / overall_s.max(1e-9)
    );

    println!(
        "STEADY: {:.3} Gbps of payload at full concurrency (tails excluded)",
        steady_bps / 1e9
    );
    if let Some(n) = mininet::eth_stats() {
        // Frames as the NIC counts them, headers included: the wire's view.
        println!(
            "WIRE: {:.3} Gbps of frames over the run ({} bytes received)",
            n.ibytes as f64 * 8.0 / 1e9 / overall_s.max(1e-9),
            n.ibytes
        );
    }

    // Completeness. Two things must hold, and they are separate questions:
    // every range was actually requested (a connection that never opened
    // abandons its range silently), and enough bytes arrived to cover them.
    let ranges_ok = conns_clean == conns_total && conns_total > 0;
    let planned_conns = (n_workers as u64) * blocks_per_worker();
    let covered = conns_total == planned_conns;

    let s = mininet::stats::snapshot();

    println!();
    println!(
        "connections   : {}/{} closed cleanly, {} failed",
        conns_clean, conns_total, s.conns_failed
    );
    println!(
        "misrouted rx  : {} packets dropped (expected 0)",
        s.misrouted_drops
    );
    println!(
        "http status   : {} non-206 responses (expected 0){}",
        bad,
        if bad > 0 {
            // Without naming it, throttling reads as lost bytes.
            if first_bad == 503 {
                " — 503 SlowDown, S3 is throttling"
            } else {
                " — see first code below"
            }
        } else {
            ""
        }
    );
    if bad > 0 {
        println!("first bad code: {}", first_bad);
    }
    println!(
        "response heads: {} did not match the range requested (expected 0)",
        hdr_bad
    );
    println!(
        "tx            : {} dropped for no mbuf (expected 0), {} held a poll for a full ring",
        s.tx_alloc_fail, s.tx_burst_fail
    );

    if let Some(n) = mininet::eth_stats() {
        println!(
            "nic rx        : {} pkts, {} imissed, {} ierrors, {} nombuf",
            n.ipackets, n.imissed, n.ierrors, n.rx_nombuf
        );
        println!("nic tx        : {} pkts, {} oerrors", n.opackets, n.oerrors);
        if n.imissed > 0 || n.rx_nombuf > 0 {
            println!("  ^ the NIC dropped frames before any queue saw them");
        }
    }
    // `setup` is SYN to Established (the network); `dial` is the CPU before it.
    // One number used to conflate them, as dial cost times conns^2/2.
    println!(
        "SETUP STATS   : conns={} failed={} us_avg={} us_p50={} us_p90={} us_max={}{}",
        s.setup.n,
        s.conns_failed,
        s.setup.us_avg,
        s.setup.us_p50,
        s.setup.us_p90,
        s.setup.us_max,
        // smoltcp exposes no retransmit count; the first one is a second out.
        if s.setup.us_max >= 1_000_000 {
            " — a SYN was retransmitted"
        } else {
            ""
        }
    );
    println!(
        "DIAL STATS    : dials={} us_avg={} us_p50={} us_p90={} us_max={} loop_ms={:.1}",
        s.dial.n,
        s.dial.us_avg,
        s.dial.us_p50,
        s.dial.us_p90,
        s.dial.us_max,
        dial_ns_max as f64 / 1e6
    );
    // How long workers ran below their concurrency for want of blocks: the
    // part of the wall time that one slow connection, not the stack, decides.
    println!(
        "TAIL STATS    : idle_max_ms={:.1} idle_avg_ms={:.1}",
        tail_ns_max as f64 / 1e6,
        tail_ns_sum as f64 / 1e6 / (n_workers.max(1) as f64)
    );
    println!("syn redials   : {}", redials);
    println!(
        "blocks        : {}/{} of {} MiB requested ({} bytes)",
        conns_total,
        planned_conns,
        BLOCK_SIZE / (1024 * 1024),
        total_expected
    );

    println!("bytes         : {} plaintext body", total_b);

    let bytes_ok = total_b == total_expected;

    if ranges_ok && covered && bytes_ok && s.misrouted_drops == 0 && hdr_bad == 0 {
        println!("COMPLETE: {} bytes, byte-exact", total_b);
    } else {
        println!(
            "INCOMPLETE: {} connections abandoned, {} blocks unrequested, {} bytes {}",
            conns_total - conns_clean,
            planned_conns.saturating_sub(conns_total),
            if total_b > total_expected {
                total_b - total_expected
            } else {
                total_expected - total_b
            },
            if total_b > total_expected { "over" } else { "short" },
        );
        if hdr_bad > 0 {
            println!(
                "  and {} response head(s) did not describe the range requested",
                hdr_bad
            );
        }
    }
}

fn cpu_count() -> usize {
    // Workers are pinned to cpus 0..n; the sampler takes the next one if the
    // machine has it. Reported through the stack's own count.
    mininet::cpu_count()
}

/// Bytes on the wire over the one window in which every slot on the machine
/// was busy, and the best second anywhere: the line-rate claim, measured off
/// the NIC's counter with a single clock.
fn wire_windows(results: &[Arc<WorkerResult>], samples: &[(u64, u64)], epoch_ns: u64) {
    if samples.len() < 2 {
        println!("WIRE STEADY: no samples");
        return;
    }
    let full = results.iter().map(|r| r.full_abs_ns.load(Ordering::Relaxed)).max().unwrap_or(0);
    let idle = results.iter().map(|r| r.idle_abs_ns.load(Ordering::Relaxed)).min().unwrap_or(0);
    // Nearest sample at or after `t`.
    let at = |t: u64| samples.iter().find(|(ts, _)| *ts >= t).copied();
    match (at(full), at(idle)) {
        (Some((t0, b0)), Some((t1, b1))) if t1 > t0 => {
            println!(
                "WIRE STEADY: {:.3} Gbps of frames from {:.3} s to {:.3} s, every slot busy ({} B)",
                (b1 - b0) as f64 * 8.0 / 1e9 / ((t1 - t0) as f64 / 1e9),
                (t0 - epoch_ns) as f64 / 1e9,
                (t1 - epoch_ns) as f64 / 1e9,
                b1 - b0
            );
        }
        _ => println!("WIRE STEADY: no window in which every slot was busy"),
    }
    // Best one-second window over the whole run.
    let mut best = 0f64;
    let mut j = 0;
    for i in 0..samples.len() {
        while j < samples.len() && samples[j].0 < samples[i].0 + 1_000_000_000 {
            j += 1;
        }
        if j < samples.len() {
            let dt = (samples[j].0 - samples[i].0) as f64 / 1e9;
            let rate = (samples[j].1 - samples[i].1) as f64 * 8.0 / 1e9 / dt;
            if rate > best {
                best = rate;
            }
        }
    }
    println!("WIRE PEAK: {:.3} Gbps of frames over the best second", best);
}

/// Do not power off: keep the serial output visible on the console.
fn exit() -> ! {
    loop {
        core::hint::spin_loop()
    }
}
