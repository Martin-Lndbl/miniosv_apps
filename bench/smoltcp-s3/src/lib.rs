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

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use mininet::print::BufWriter;
use mininet::{println, thread, Config, Endpoint, Request, Stack, Worker, WorkerConfig, WorkerHandle};

use config::{
    BLOCK_SIZE, CONNS_PER_WORKER, N_WORKERS_REQ, OBJECT_SIZE, PLAIN_HTTP, STUB_TLS_AFTER_HANDSHAKE,
    TARGET_HOST, TARGET_IP, TARGET_PATH,
};

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

/// One worker: open every connection it has a port for, then poll until they
/// have all finished.
fn run_worker(handle: WorkerHandle, peer: Endpoint, first_block: u64, out: Arc<WorkerResult>) {
    let queue_id = handle.queue_id();

    let mut cfg = WorkerConfig::new(peer);
    cfg.conns = CONNS_PER_WORKER;
    let mut w = match Worker::new(handle, &cfg) {
        Ok(w) => w,
        Err(e) => {
            println!("FAIL: q{}: {}", queue_id, e);
            return;
        }
    };

    // Fault injection skips opening the last few slots, leaving their byte
    // ranges genuinely unrequested -- exactly the failure the completeness
    // check has to catch.
    let open = w.slots().saturating_sub(FAULT_ABANDON_CONNS);
    // What each slot asked for, so the response head can be checked against
    // it afterwards. `None` for a slot that never opened.
    let mut asked: Vec<Option<(u64, u64)>> = alloc::vec![None; w.slots()];
    let mut expected: u64 = 0;
    // smoltcp holds every SYN until the `poll` after this loop, so time spent
    // here is time no handshake could start.
    let dial_start_ns = w.clock().elapsed_ns();
    for i in 0..open {
        let (start, end) = block_range(first_block + i as u64);
        let mut head = [0u8; 384];
        let n = build_range_request(&mut head, start, end);
        let req = Request {
            head: &head[..n],
            discard_ciphertext: STUB_TLS_AFTER_HANDSHAKE,
        };
        if w.connect(i, &req).is_err() {
            continue;
        }
        asked[i] = Some((start, end));
        expected += end - start + 1;
    }

    let start_ns = w.clock().elapsed_ns();
    let dial_ns = start_ns.saturating_sub(dial_start_ns);
    while !w.poll() {}
    let elapsed_ns = w.clock().elapsed_ns().saturating_sub(start_ns);

    let mut bytes = 0u64;
    let mut total = 0u64;
    let mut clean = 0u64;
    let mut bad = 0u64;
    let mut first_bad = 0u64;
    let mut hdr_bad = 0u64;
    for slot in 0..w.slots() {
        let c = match w.conn(slot) {
            Some(c) => c,
            None => continue,
        };
        bytes += c.body_bytes();
        total += 1;
        if c.is_complete() {
            clean += 1;
        } else {
            // A worker that could not finish part of its range never delivered
            // those bytes -- surface it here, not as a silent gap.
            println!(
                "q{}: conn on port {} did not close cleanly ({} B)",
                queue_id,
                c.src_port(),
                c.body_bytes()
            );
        }
        // The request is ranged, so only 206 is what we asked for. With the
        // record layer stubbed out there is no plaintext head to read, so
        // there is no status to judge, and no headers either.
        if !c.headers_parsed() {
            continue;
        }
        if c.status() != 206 {
            bad += 1;
            if first_bad == 0 {
                first_bad = c.status() as u64;
            }
        }
        // Check the parsed head against what this slot actually requested.
        // The parser is new and nothing else here reads it, so without this
        // it would be exercised on every run and validated on none -- and a
        // misparsed Content-Length is exactly the bug that would later hand
        // DuckDB a short page without anything noticing.
        if let Some((start, end)) = asked[slot] {
            let want = end - start + 1;
            let head = c.head();
            let len_ok = head.content_length == Some(want);
            let range_ok = head
                .content_range
                .map(|r| r.first == start && r.last == end && r.total == Some(OBJECT_SIZE))
                .unwrap_or(false);
            // S3 always sends one on a 206; its absence is a parse failure,
            // not a server that chose not to.
            let etag_ok = head.etag.is_some();
            if !len_ok || !range_ok || !etag_ok {
                hdr_bad += 1;
                if hdr_bad == 1 {
                    println!(
                        "q{}: head mismatch on port {}: content-length {:?} (want {}), \
                         content-range {:?} (want {}-{}/{}), etag {:?}",
                        queue_id,
                        c.src_port(),
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

    out.bytes_received.store(bytes, Ordering::Relaxed);
    out.elapsed_ns.store(elapsed_ns, Ordering::Relaxed);
    out.dial_ns.store(dial_ns, Ordering::Relaxed);
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
    println!(
        "bench: {} workers x {} conns x {} MiB block, tls_stub={} scheme={}",
        N_WORKERS_REQ,
        CONNS_PER_WORKER,
        BLOCK_SIZE / (1024 * 1024),
        STUB_TLS_AFTER_HANDSHAKE,
        if PLAIN_HTTP { "http" } else { "https" }
    );

    let peer = Endpoint::new(TARGET_IP, TARGET_HOST, !PLAIN_HTTP);
    let t = peer.ip;
    println!(
        "target: {}.{}.{}.{}:{} {}",
        t[0], t[1], t[2], t[3], peer.port, TARGET_HOST
    );
    if !peer.is_configured() {
        println!("FAIL: AWS_TARGET_IP is unset or malformed — run `just setup smoltcp-s3`");
        exit();
    }
    if OBJECT_SIZE == 0 || BLOCK_SIZE == 0 || CONNS_PER_WORKER == 0 || N_WORKERS_REQ == 0 {
        println!("FAIL: BENCH_WORKERS, BENCH_CONNS_PER_WORKER, BENCH_BLOCK_SIZE and AWS_BUCKET_SIZE must be nonzero");
        exit();
    }

    let stack = match Stack::up(&Config {
        queues: N_WORKERS_REQ,
    }) {
        Ok(s) => s,
        Err(e) => {
            println!("FAIL: {}", e);
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
    let threads: Vec<_> = (0..n_workers)
        .map(|i| {
            let handle = stack.handle(i).expect("queue granted by the device");
            let peer = peer.clone();
            let out = results[i as usize].clone();
            let first_block = (i as u64) * (CONNS_PER_WORKER as u64);
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

    report(&results, n_workers, overall_ns);

    stack.down();
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

    for (i, r) in results.iter().enumerate() {
        let b = r.bytes_received.load(Ordering::Relaxed);
        let e = r.elapsed_ns.load(Ordering::Relaxed) as f64 / 1e9;
        dial_ns_max = dial_ns_max.max(r.dial_ns.load(Ordering::Relaxed));
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

    // Completeness. Two things must hold, and they are separate questions:
    // every range was actually requested (a connection that never opened
    // abandons its range silently), and enough bytes arrived to cover them.
    let ranges_ok = conns_clean == conns_total && conns_total > 0;
    let planned_conns = (n_workers as u64) * (CONNS_PER_WORKER as u64);
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
        "response heads: {} did not match the range requested (expected 0){}",
        hdr_bad,
        if STUB_TLS_AFTER_HANDSHAKE {
            " — not checked, the record layer is stubbed out"
        } else {
            ""
        }
    );
    println!(
        "tx drops      : {} no-mbuf, {} ring-full (expected 0)",
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
    let (mut qi, mut qe) = ([0u64; 32], [0u64; 32]);
    let nq = mininet::eth_qstats(&mut qi, &mut qe);
    for q in 0..nq.min(n_workers as usize) {
        if qe[q] > 0 {
            println!("  q{}: {} rx pkts, {} errors", q, qi[q], qe[q]);
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
    println!(
        "blocks        : {}/{} of {} MiB requested ({} bytes)",
        conns_total,
        planned_conns,
        BLOCK_SIZE / (1024 * 1024),
        total_expected
    );

    if STUB_TLS_AFTER_HANDSHAKE {
        // Ciphertext, so it carries TLS record and HTTP header overhead and
        // cannot be compared byte-for-byte against the plaintext ranges.
        let overhead = total_b as f64 - total_expected as f64;
        println!(
            "bytes         : {} on the wire (ciphertext, {:+.2}%)",
            total_b,
            overhead * 100.0 / (total_expected.max(1)) as f64
        );
    } else {
        println!("bytes         : {} plaintext body", total_b);
    }

    let bytes_ok = if STUB_TLS_AFTER_HANDSHAKE {
        total_b >= total_expected
    } else {
        total_b == total_expected
    };

    if ranges_ok && covered && bytes_ok && s.misrouted_drops == 0 && hdr_bad == 0 {
        if STUB_TLS_AFTER_HANDSHAKE {
            println!(
                "COMPLETE: all {} blocks fetched (set BENCH_TLS_STUB=0 for a byte-exact check)",
                conns_total
            );
        } else {
            println!("COMPLETE: {} bytes, byte-exact", total_b);
        }
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

/// Do not power off: keep the serial output visible on the console.
fn exit() -> ! {
    loop {
        core::hint::spin_loop()
    }
}
