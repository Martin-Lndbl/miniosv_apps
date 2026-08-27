//! Exercises `mininet::Service`: the request queue, the parked-thread
//! handoff, and delivery into a caller's buffer.
//!
//! Selected with `BENCH_MODE=net`. The benchmark proper cannot cover any of
//! this -- it drives workers directly, counts bytes and never looks at one --
//! so these are the checks that would otherwise only be made by DuckDB, in a
//! place where a failure is much harder to read.
//!
//! The interesting assertion is #3. Everything the benchmark measures would
//! pass just as happily if the stack returned the *wrong* 64 KiB, because a
//! byte count cannot tell one range from another. Fetching overlapping ranges
//! and requiring the overlap to agree is what ties delivered bytes to the
//! offset that was asked for.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use mininet::print::BufWriter;
use mininet::{println, thread, Config, Endpoint, Error, Service, ServiceConfig, Stack};

use crate::config::{PLAIN_HTTP, STUB_TLS_AFTER_HANDSHAKE, TARGET_HOST, TARGET_IP, TARGET_PATH};

const K: usize = 1024;

fn range_request(buf: &mut [u8], start: u64, end: u64) -> usize {
    let mut w = BufWriter::new(buf);
    let _ = write!(
        &mut w,
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: mininet-selftest/0.1\r\nRange: bytes={}-{}\r\nConnection: close\r\n\r\n",
        TARGET_PATH, TARGET_HOST, start, end,
    );
    w.used()
}

/// One ranged GET into `buf`. Returns bytes delivered, or the failure.
fn get(svc: &Service, start: u64, end: u64, buf: &mut [u8]) -> Result<u64, Error> {
    let mut head = [0u8; 384];
    let n = range_request(&mut head, start, end);
    let r = svc.get_with(&head[..n], buf, STUB_TLS_AFTER_HANDSHAKE)?;
    let want = end - start + 1;
    if r.status != 206 {
        println!("  status {} (want 206)", r.status);
        return Err(Error::BadResponse);
    }
    if r.written != want {
        println!("  wrote {} of {}", r.written, want);
        return Err(Error::BadResponse);
    }
    if r.content_length != Some(want) {
        println!("  content-length {:?} (want {})", r.content_length, want);
        return Err(Error::BadResponse);
    }
    Ok(r.written)
}

/// Prints and tallies. Returns whether it passed.
fn check(name: &str, ok: bool, failures: &mut u32) -> bool {
    if ok {
        println!("ok   {}", name);
    } else {
        println!("FAIL {}", name);
        *failures += 1;
    }
    ok
}

pub fn run() -> ! {
    println!("mininet selftest: Service, parking handoff, BufferSink");

    let peer = Endpoint::new(TARGET_IP, TARGET_HOST, !PLAIN_HTTP);
    let stack = match Stack::up(&Config { queues: 4 }) {
        Ok(s) => s,
        Err(e) => {
            println!("FAIL: stack: {}", e);
            halt();
        }
    };

    let mut cfg = ServiceConfig::new(peer);
    cfg.conns_per_worker = 4;
    // 64 KiB ranges; the 4 MiB default per socket is pointless here and would
    // cost 64 MiB of buffers for a test that moves less than one.
    cfg.rx_buffer = 256 * K;
    let svc = match Service::start(&stack, &cfg) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            println!("FAIL: service: {}", e);
            halt();
        }
    };
    println!("service: {} workers x {} conns", stack.queues(), cfg.conns_per_worker);

    let mut failures = 0u32;
    let mut a = vec![0u8; 64 * K];
    let mut b = vec![0u8; 64 * K];

    // 1. A single request completes at all: submitted from this thread, served
    //    on another, this thread parked in between.
    let one = get(&svc, 0, 64 * K as u64 - 1, &mut a).is_ok();
    check("single ranged GET delivers 65536 bytes", one, &mut failures);

    // 2. The same range twice gives the same bytes. Catches a body that is
    //    delivered but scrambled -- interleaved records, a mis-drained ring.
    let two = get(&svc, 0, 64 * K as u64 - 1, &mut b).is_ok();
    check(
        "the same range fetched twice is byte-identical",
        two && a == b,
        &mut failures,
    );

    // 3. An overlapping range agrees where it overlaps. This is what ties the
    //    bytes to the offset: a stack that ignored Range entirely, or was off
    //    by a block, passes every other check here and fails this one.
    let mut c = vec![0u8; 32 * K];
    let three = get(&svc, 32 * K as u64, 64 * K as u64 - 1, &mut c).is_ok();
    check(
        "an overlapping range agrees with the first on the overlap",
        three && c[..] == a[32 * K..],
        &mut failures,
    );

    // 4. A buffer that cannot hold the response is an error, not a silent
    //    truncation and not a write past the end.
    let mut small = vec![0u8; K];
    let four = matches!(
        {
            let mut head = [0u8; 384];
            let n = range_request(&mut head, 0, 64 * K as u64 - 1);
            svc.get_with(&head[..n], &mut small, STUB_TLS_AFTER_HANDSHAKE)
        },
        Err(Error::BufferTooSmall)
    );
    check("a too-small buffer fails instead of truncating", four, &mut failures);

    // 5. Many threads submitting at once. Each fetches a distinct range and
    //    checks it against a range this thread already has, so a request
    //    answered with another request's body is caught rather than counted.
    const N: u64 = 8;
    let mut handles = alloc::vec::Vec::new();
    let expect = Arc::new(a.clone());
    let bad = Arc::new(AtomicU32::new(0));
    for i in 0..N {
        let svc = svc.clone();
        let expect = expect.clone();
        let bad = bad.clone();
        handles.push(thread::spawn(
            move || {
                // Every thread asks for the same first 8 KiB, from whichever
                // worker the round-robin lands on; all must agree.
                let mut buf = vec![0u8; 8 * K];
                let ok = get(&svc, 0, 8 * K as u64 - 1, &mut buf).is_ok()
                    && buf[..] == expect[..8 * K];
                if !ok {
                    println!("  thread {} disagreed", i);
                    bad.fetch_add(1, Ordering::Relaxed);
                }
            },
            None,
        ));
    }
    for h in handles {
        h.join();
    }
    check(
        "8 concurrent submitters each get the right body",
        bad.load(Ordering::Relaxed) == 0,
        &mut failures,
    );

    println!();
    if failures == 0 {
        println!("COMPLETE: selftest passed");
    } else {
        println!("INCOMPLETE: {} selftest check(s) failed", failures);
    }
    halt();
}

fn halt() -> ! {
    loop {
        core::hint::spin_loop()
    }
}
