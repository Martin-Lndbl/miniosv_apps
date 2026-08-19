#![no_std]
#![allow(non_camel_case_types)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::ffi::{c_int, c_void};
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Address};

use rustls::client::{ClientConfig, UnbufferedClientConnection};
use rustls::pki_types::{ServerName, UnixTime};
use rustls::time_provider::TimeProvider;
use rustls::unbuffered::{ConnectionState, UnbufferedStatus};
use rustls::RootCertStore;

// ---------------------------------------------------------------------------
// Rust global allocator routed to OSv's C malloc via the shim. rustls,
// rustcrypto, webpki, and Arc<T> all need a heap.
// ---------------------------------------------------------------------------

extern "C" {
    fn shim_malloc(size: u64) -> *mut u8;
    fn shim_free(ptr: *mut u8);
    fn shim_realloc(ptr: *mut u8, size: u64) -> *mut u8;
    fn shim_time_seconds() -> u64;
    fn shim_time_ns() -> u64;
}

struct ShimAllocator;

// malloc gives 16-byte alignment; over-align by stashing the raw pointer
// just before the aligned slot when a caller wants more.
unsafe impl GlobalAlloc for ShimAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() <= 16 {
            return unsafe { shim_malloc(layout.size() as u64) };
        }
        let extra = layout.align() + core::mem::size_of::<*mut u8>();
        let raw = unsafe { shim_malloc((layout.size() + extra) as u64) };
        if raw.is_null() {
            return raw;
        }
        let raw_addr = raw as usize + core::mem::size_of::<*mut u8>();
        let aligned = (raw_addr + layout.align() - 1) & !(layout.align() - 1);
        unsafe {
            *((aligned - core::mem::size_of::<*mut u8>()) as *mut *mut u8) = raw;
        }
        aligned as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.align() <= 16 {
            unsafe { shim_free(ptr) };
        } else {
            unsafe {
                let slot = (ptr as usize - core::mem::size_of::<*mut u8>()) as *mut *mut u8;
                shim_free(*slot);
            }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.align() <= 16 {
            return unsafe { shim_realloc(ptr, new_size as u64) };
        }
        let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
        let new_ptr = unsafe { self.alloc(new_layout) };
        if !new_ptr.is_null() {
            let copy = core::cmp::min(layout.size(), new_size);
            unsafe { core::ptr::copy_nonoverlapping(ptr, new_ptr, copy) };
            unsafe { self.dealloc(ptr, layout) };
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: ShimAllocator = ShimAllocator;

// ---------------------------------------------------------------------------
// Console (no_std, no libc stdio; we link OSv's `write(2)`).
// ---------------------------------------------------------------------------

extern "C" {
    fn write(fd: c_int, buf: *const u8, count: usize) -> isize;
}

/// Formats a whole line into a stack buffer so it can be handed to the console
/// in exactly one write. `write_fmt` calls `write_str` once per format
/// fragment, so writing directly let concurrent workers interleave *within* a
/// line, producing output like "q2: 2048 of 16384 ... using q245".
struct BufWriter<'a> {
    buf: &'a mut [u8],
    used: usize,
}

const LINE_MAX: usize = 256;

impl<'a> BufWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, used: 0 }
    }
}

impl Write for BufWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = core::cmp::min(s.len(), self.buf.len() - self.used);
        self.buf[self.used..self.used + n].copy_from_slice(&s.as_bytes()[..n]);
        self.used += n;
        Ok(()) // silently truncate rather than fail a diagnostic print
    }
}

/// Guards the console so two workers cannot interleave their lines. Printing
/// happens at startup and teardown only, so spinning here costs nothing on the
/// data path.
static PRINT_LOCK: AtomicBool = AtomicBool::new(false);

fn print_line(args: fmt::Arguments) {
    let mut buf = [0u8; LINE_MAX];
    let mut used = {
        let mut w = BufWriter::new(&mut buf);
        let _ = w.write_fmt(args);
        w.used
    };
    if used == LINE_MAX {
        used -= 1; // make room for the newline on a truncated line
    }
    buf[used] = b'\n';
    used += 1;

    while PRINT_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let mut off = 0;
    while off < used {
        let n = unsafe { write(1, buf[off..].as_ptr(), used - off) };
        if n <= 0 {
            break;
        }
        off += n as usize;
    }
    PRINT_LOCK.store(false, Ordering::Release);
}

macro_rules! println {
    () => { print_line(format_args!("")) };
    ($($arg:tt)*) => { print_line(format_args!($($arg)*)) };
}

// ---------------------------------------------------------------------------
// minidpdk FFI (matches rust_app/shim/shim.hh).
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct rte_pktmbuf_pool {
    _private: [u8; 0],
}

extern "C" {
    fn shim_get_dev_info(port_id: u16, max_rx_queues: *mut u16,
                         max_tx_queues: *mut u16) -> c_int;
    fn shim_pktmbuf_pool_create(
        name: *const u8,
        n: u32,
        cache_size: u32,
        priv_size: u16,
        data_room_size: u16,
    ) -> *mut rte_pktmbuf_pool;
    fn shim_mempool_free(pool: *mut rte_pktmbuf_pool);
    fn shim_eth_dev_configure(port_id: u16, nb_rx_q: u16, nb_tx_q: u16) -> c_int;
    fn shim_adjust_nb_rx_tx_desc(port_id: u16, nb_rx_desc: *mut u16, nb_tx_desc: *mut u16);
    fn shim_rx_queue_setup(
        port_id: u16,
        queue_id: u16,
        nb_desc: u16,
        mempool: *mut rte_pktmbuf_pool,
    ) -> c_int;
    fn shim_tx_queue_setup(port_id: u16, queue_id: u16, nb_desc: u16) -> c_int;
    fn shim_dev_start(port_id: u16) -> c_int;
    fn shim_dev_stop(port_id: u16);
    fn shim_thread_spawn(
        f: extern "C" fn(*mut c_void),
        arg: *mut c_void,
        cpu_id: c_int,
    ) -> *mut c_void;
    fn shim_thread_join(handle: *mut c_void);
    fn shim_macaddr_get(port_id: u16, addr_bytes: *mut u8);

    fn shim_mbuf_alloc_tx(
        pool: *mut rte_pktmbuf_pool,
        queue_id: u16,
        out_handle: *mut *mut c_void,
        out_cap: *mut u16,
    ) -> *mut u8;
    fn shim_mbuf_tx(port_id: u16, queue_id: u16, handle: *mut c_void, len: u16) -> c_int;
    fn shim_mbuf_free(handle: *mut c_void);
    fn shim_rss_hash_key(port_id: u16, out_key: *mut u8, out_len: u16) -> c_int;
    fn shim_rss_reta(port_id: u16, out: *mut u16, out_entries: u16) -> c_int;
    fn shim_eth_stats(port_id: u16, out: *mut u64, n: u16) -> c_int;
    fn shim_eth_qstats(port_id: u16, ipkts: *mut u64, errs: *mut u64, nq: u16) -> c_int;
    fn shim_mbuf_rx_burst_n(
        port_id: u16,
        queue_id: u16,
        out_handles: *mut *mut c_void,
        out_data: *mut *const u8,
        out_lens: *mut u16,
        max: u16,
    ) -> u16;
}

struct PktPool(*mut rte_pktmbuf_pool);

impl Drop for PktPool {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { shim_mempool_free(self.0) };
        }
    }
}

/// Configure & start port 0 with `n_queues` RX+TX queues (RSS on the
/// TCP/IPv4 4-tuple when n_queues > 1). Returns one mempool per queue
/// plus the MAC. AWS gives the guest exactly one ENA interface so
/// hard-coding port 0 is fine.
///
/// Per-queue mempools eliminate cross-worker contention on the pool
/// spinlock, which choked throughput badly at N=8 and made it unstable
/// at N=4 under the shared-pool design.
fn probe_and_open(n_queues: u16) -> Option<(Vec<PktPool>, [u8; 6])> {
    const PORT: u16 = 0;
    const DATA_ROOM_SIZE: u16 = 1536;
    // 1024 descriptors could not absorb the inbound burst while a worker
    // walked 48 connection state machines between polls: the NIC dropped 3.3%
    // of all inbound frames for want of a free descriptor (imissed), which is
    // invisible above the driver and looked like unanswered SYNs.
    const DESC_NUM: u16 = 4096;
    const CACHE: u32 = 64;
    // Size for every simultaneous holder rather than the RX ring plus a
    // guess: the RX ring pins DESC_NUM-1, the TX ring holds up to DESC_NUM
    // more awaiting reclaim, the per-core cache holds CACHE, and the RX
    // prefetch holds RX_BURST. At DESC_NUM*2 the pool fell ~100 mbufs short
    // of that worst case, and exhaustion silently discarded frames — a
    // dropped SYN is then indistinguishable from an unanswered one.
    let per_queue_size: u32 =
        (DESC_NUM as u32) * 2 + CACHE + RX_BURST as u32 + 512;

    let mut pools: Vec<PktPool> = Vec::with_capacity(n_queues as usize);
    for q in 0..n_queues {
        let mut name = [0u8; 32];
        let _ = write!(&mut BufWriter::new(&mut name), "bench-pool-{}\0", q);
        let raw = unsafe {
            shim_pktmbuf_pool_create(name.as_ptr(), per_queue_size, CACHE, 0, DATA_ROOM_SIZE)
        };
        if raw.is_null() {
            return None;
        }
        pools.push(PktPool(raw));
    }

    if unsafe { shim_eth_dev_configure(PORT, n_queues, n_queues) } != 0 {
        return None;
    }
    let (mut rx_desc, mut tx_desc) = (DESC_NUM, DESC_NUM);
    unsafe { shim_adjust_nb_rx_tx_desc(PORT, &mut rx_desc, &mut tx_desc) };
    // The device clamps to what it supports, so report what was actually
    // granted rather than what was asked for.
    if rx_desc != DESC_NUM || tx_desc != DESC_NUM {
        println!("descriptors: asked {}, got rx {} tx {} (device clamp)",
            DESC_NUM, rx_desc, tx_desc);
    } else {
        println!("descriptors: rx {} tx {} per queue", rx_desc, tx_desc);
    }
    for q in 0..n_queues {
        if unsafe { shim_rx_queue_setup(PORT, q, rx_desc, pools[q as usize].0) } != 0 {
            return None;
        }
        if unsafe { shim_tx_queue_setup(PORT, q, tx_desc) } != 0 {
            return None;
        }
    }
    if unsafe { shim_dev_start(PORT) } != 0 {
        return None;
    }
    let mut mac = [0u8; 6];
    unsafe { shim_macaddr_get(PORT, mac.as_mut_ptr()) };
    println!(
        "port 0: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ({} queues)",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], n_queues
    );
    Some((pools, mac))
}

// ---------------------------------------------------------------------------
// Compile-time configuration, baked in from the environment by `just setup`.
// Defaults are the values the benchmark was tuned with, so an unset variable
// still gives a sensible run.
// ---------------------------------------------------------------------------

/// Decimal integer, optionally with an IEC suffix (K/M/G/T, `i` and `B`
/// ignored) so `AWS_BUCKET_SIZE="10G"` parses directly.
const fn parse_size(s: Option<&str>, default: u64) -> u64 {
    let s = match s { Some(s) => s, None => return default };
    let b = s.as_bytes();
    if b.is_empty() { return default; }
    let mut i = 0;
    let mut v: u64 = 0;
    while i < b.len() {
        let c = b[i];
        if c >= b'0' && c <= b'9' {
            v = v * 10 + (c - b'0') as u64;
            i += 1;
        } else {
            let mult: u64 = match c {
                b'K' | b'k' => 1024,
                b'M' | b'm' => 1024 * 1024,
                b'G' | b'g' => 1024 * 1024 * 1024,
                b'T' | b't' => 1024u64 * 1024 * 1024 * 1024,
                b'B' | b'b' | b'i' | b'I' => 1,
                _ => return default,
            };
            return v * mult;
        }
    }
    v
}

const fn parse_bool(s: Option<&str>, default: bool) -> bool {
    let s = match s { Some(s) => s, None => return default };
    let b = s.as_bytes();
    if b.is_empty() { return default; }
    match b[0] {
        b'1' | b't' | b'T' | b'y' | b'Y' => true,
        b'0' | b'f' | b'F' | b'n' | b'N' => false,
        _ => default,
    }
}

const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] { return false; }
        i += 1;
    }
    true
}

/// `BENCH_SCHEME`. Unset means https.
const fn is_http(s: Option<&str>) -> bool {
    let b = match s { Some(s) => s.as_bytes(), None => return false };
    bytes_eq(b, b"http")
}

const fn scheme_ok(s: Option<&str>) -> bool {
    let b = match s { Some(s) => s.as_bytes(), None => return true };
    b.is_empty() || bytes_eq(b, b"http") || bytes_eq(b, b"https")
}

/// Dotted-quad, e.g. "3.5.216.240". Falls back to 0.0.0.0, which the caller
/// treats as "not configured" and reports rather than silently misdialling.
const fn parse_ipv4(s: Option<&str>) -> [u8; 4] {
    let s = match s { Some(s) => s, None => return [0; 4] };
    let b = s.as_bytes();
    let mut out = [0u8; 4];
    let mut oct = 0;
    let mut acc: u32 = 0;
    let mut seen = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c >= b'0' && c <= b'9' {
            acc = acc * 10 + (c - b'0') as u32;
            if acc > 255 { return [0; 4]; }
            seen = true;
        } else if c == b'.' {
            if !seen || oct >= 3 { return [0; 4]; }
            out[oct] = acc as u8;
            oct += 1;
            acc = 0;
            seen = false;
        } else {
            return [0; 4];
        }
        i += 1;
    }
    if !seen || oct != 3 { return [0; 4]; }
    out[3] = acc as u8;
    out
}

/// RSS queues (and worker threads) to ask for; clamped to the device maximum.
const N_WORKERS_REQ: u16 = parse_size(option_env!("BENCH_WORKERS"), 8) as u16;
/// Parallel TLS connections each worker drives.
const CONNS_PER_WORKER: usize =
    parse_size(option_env!("BENCH_CONNS_PER_WORKER"), 24) as usize;
/// Size of the object in the bucket. Bounds the offsets a Range may name; it
/// is no longer the amount transferred.
const OBJECT_SIZE: u64 = parse_size(option_env!("AWS_BUCKET_SIZE"), 10 * 1024 * 1024 * 1024);
/// Bytes each connection requests. Held fixed while worker count varies, so
/// that a throughput-vs-parallelism curve is not also a
/// throughput-vs-request-size curve: splitting a fixed total across workers
/// shrank each request as parallelism rose and confounded the two.
const BLOCK_SIZE: u64 = parse_size(option_env!("BENCH_BLOCK_SIZE"), 64 * 1024 * 1024);
/// `BENCH_SCHEME=http` drops TLS entirely and dials port 80, which isolates the
/// network stack from the record layer. Rows from the two schemes measure
/// different things and do not belong in one CSV.
const PLAIN_HTTP: bool = is_http(option_env!("BENCH_SCHEME"));
/// A misspelt scheme would silently run TLS and produce a row labelled https,
/// which is a wrong measurement rather than a failed one.
const _: () = assert!(
    scheme_ok(option_env!("BENCH_SCHEME")),
    "BENCH_SCHEME must be http or https"
);
/// Discard ciphertext after the handshake instead of decrypting it. Isolates
/// the network stack from the record layer; the transfer is then unverifiable
/// byte-for-byte, so the completeness check falls back to a range check.
/// Meaningless without a record layer, so `PLAIN_HTTP` forces it off.
const STUB_TLS_AFTER_HANDSHAKE: bool =
    parse_bool(option_env!("BENCH_TLS_STUB"), false) && !PLAIN_HTTP;

// ---------------------------------------------------------------------------
// Counters. These are cheap and stay in the build: each one is an invariant
// that should hold on every run, so a regression shows up in the output
// rather than as a mysteriously slower number.
// ---------------------------------------------------------------------------

/// Packets discarded because they arrived on a queue that does not own the
/// destination port. Source ports are chosen so this cannot happen; a nonzero
/// value means the RSS model no longer matches the hardware.
static MISROUTED_DROPS: AtomicU64 = AtomicU64::new(0);
static CONNS_ESTABLISHED: AtomicU64 = AtomicU64::new(0);
static CONNS_FAILED: AtomicU64 = AtomicU64::new(0);
/// Extra SYNs beyond the first. Should be 0.
static SYN_RETRIES: AtomicU64 = AtomicU64::new(0);
/// Frames dropped before reaching the NIC: no mbuf available, or the TX ring
/// refused them. Both are invisible to smoltcp, which believes it sent them.
static TX_ALLOC_FAIL: AtomicU64 = AtomicU64::new(0);
static TX_BURST_FAIL: AtomicU64 = AtomicU64::new(0);
static SETUP_MS_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Non-206 responses, and the first such code. 0 means none: not a status.
static BAD_STATUS: AtomicU64 = AtomicU64::new(0);
static FIRST_BAD_STATUS: AtomicU64 = AtomicU64::new(0);

static N_WORKERS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// RSS steering model.
//
// The RX queue a flow lands on is a pure function of its 4-tuple:
//
//     queue = reta[ toeplitz(key, src_ip||dst_ip||sport||dport) & (len-1) ]
//
// The key and table are read from the device at startup rather than assumed.
// This exact form was validated against 200k live packets (100% agreement,
// against 48-50% for every other key orientation and tuple order tried), and
// the Toeplitz implementation against the canonical Microsoft RSS vectors.
//
// Because we control our own source port, we can invert it: pick ports whose
// return traffic provably lands on the queue we are polling.
// ---------------------------------------------------------------------------

const EPH_BASE: u16 = 49152;
const EPH_LEN: usize = 16384;
const OWNED_BITMAP_BYTES: usize = EPH_LEN / 8;

static mut RSS_KEY: [u8; 64] = [0; 64];
static mut RSS_KEY_LEN: usize = 0;
static mut RSS_RETA: [u16; 512] = [0; 512];
static mut RSS_RETA_LEN: usize = 0;

/// Toeplitz hash, sliding-window form.
fn toeplitz(key: &[u8], data: &[u8]) -> u32 {
    if key.len() < 4 { return 0; }
    let mut result: u32 = 0;
    let mut w = u32::from_be_bytes([key[0], key[1], key[2], key[3]]);
    let mut next_bit = 32usize;
    let total_key_bits = key.len() * 8;
    for &byte in data {
        for b in (0..8).rev() {
            if (byte >> b) & 1 == 1 {
                result ^= w;
            }
            let kb = if next_bit < total_key_bits {
                (key[next_bit / 8] >> (7 - (next_bit % 8))) & 1
            } else {
                0
            };
            w = (w << 1) | (kb as u32);
            next_bit += 1;
        }
    }
    result
}

/// Predicted RX queue for a 12-byte tuple in wire order. u16::MAX if the RSS
/// configuration could not be read.
static mut SINGLE_QUEUE: bool = false;

fn predict_queue(tuple: &[u8; 12]) -> u16 {
    // One queue receives everything, so there is nothing to predict. The PMD
    // does not configure RSS for a single queue, so the key and table below
    // would be unreadable anyway.
    if unsafe { SINGLE_QUEUE } {
        return 0;
    }
    let (key_len, reta_len) = unsafe { (RSS_KEY_LEN, RSS_RETA_LEN) };
    if key_len == 0 || reta_len == 0 {
        return u16::MAX;
    }
    let key = &unsafe { &*core::ptr::addr_of!(RSS_KEY) }[..key_len];
    let reta = unsafe { &*core::ptr::addr_of!(RSS_RETA) };
    let h = toeplitz(key, tuple) as usize;
    reta[h & (reta_len - 1)]
}

/// Which RX queue an inbound packet for `local_port` would be steered to.
/// Source is the target, destination is us: the direction the NIC hashes.
fn queue_for_local_port(our_ip: [u8; 4], local_port: u16) -> u16 {
    let mut tuple = [0u8; 12];
    tuple[0..4].copy_from_slice(&TARGET_IP.octets());
    tuple[4..8].copy_from_slice(&our_ip);
    tuple[8..10].copy_from_slice(&TARGET_PORT.to_be_bytes());
    tuple[10..12].copy_from_slice(&local_port.to_be_bytes());
    predict_queue(&tuple)
}

/// Mark every ephemeral port whose return traffic lands on `queue_id`.
fn build_owned_ports(our_ip: [u8; 4], queue_id: u16, bitmap: &mut [u8]) -> u32 {
    let mut count = 0u32;
    for i in 0..EPH_LEN {
        let port = EPH_BASE + i as u16;
        if queue_for_local_port(our_ip, port) == queue_id {
            bitmap[i / 8] |= 1 << (i % 8);
            count += 1;
        }
    }
    count
}

#[inline]
fn owns_port(bitmap: *const u8, port: u16) -> bool {
    if port < EPH_BASE {
        return true; // non-ephemeral (DHCP etc.) is never ours to filter
    }
    let i = (port - EPH_BASE) as usize;
    unsafe { *bitmap.add(i / 8) & (1 << (i % 8)) != 0 }
}

/// Read the key and indirection table the device is actually using.
fn load_rss_config(n_queues: u16) -> bool {
    if n_queues == 1 {
        unsafe { SINGLE_QUEUE = true };
        // Same shape as below: the harness reads worker count off this line.
        println!("rss: {} queues, steering is trivial", n_queues);
        return true;
    }
    let mut key = [0u8; 64];
    let klen = unsafe { shim_rss_hash_key(0, key.as_mut_ptr(), key.len() as u16) };
    if klen <= 0 {
        println!("FAIL: RSS hash key unavailable (rc={})", klen);
        return false;
    }
    unsafe {
        let dst = &mut *core::ptr::addr_of_mut!(RSS_KEY);
        dst[..klen as usize].copy_from_slice(&key[..klen as usize]);
        RSS_KEY_LEN = klen as usize;
    }

    let mut reta = [0u16; 512];
    let n = unsafe { shim_rss_reta(0, reta.as_mut_ptr(), reta.len() as u16) };
    if n <= 0 {
        println!("FAIL: RSS indirection table unavailable (rc={})", n);
        return false;
    }
    let n = n as usize;
    if n & (n - 1) != 0 {
        println!("FAIL: RSS table size {} is not a power of two", n);
        return false;
    }
    unsafe {
        let dst = &mut *core::ptr::addr_of_mut!(RSS_RETA);
        dst[..n].copy_from_slice(&reta[..n]);
        RSS_RETA_LEN = n;
    }
    println!("rss: {} queues, {}-entry table, {}-byte key", n_queues, n, klen);
    true
}

// ---------------------------------------------------------------------------
// smoltcp Device on minidpdk. TX writes straight into the mbuf's data
// area; RX carries the mbuf pointer through the token and frees on
// consume/drop. `pending_synth` returns one fabricated frame on the next
// poll — used to seed the neighbor cache without doing an RSS-misrouted
// ARP.
// ---------------------------------------------------------------------------

const MTU: usize = 1514;

// Port is always 0 (AWS gives one ENA interface per guest).
struct DpdkDevice {
    queue_id: u16,
    pool: *mut rte_pktmbuf_pool,
    pending_synth: Option<Vec<u8>>,
    // Bitmap over the ephemeral range of dst_ports this iface owns, i.e.
    // those whose return traffic RSS steers to this queue. Since source
    // ports are now chosen so that this always holds, a packet failing the
    // test means the prediction was wrong — it is counted, not just dropped.
    // Null means "accept all ports" (the DHCP/ARP path in learn_network).
    owned_ports: *const u8,
    // Prefetch buffer: rte_eth_rx_burst is cheap in bulk but expensive
    // per call, so we drain up to RX_BURST mbufs at once and hand them
    // to smoltcp one at a time out of these arrays.
    rx_pref_handles: [*mut c_void; RX_BURST],
    rx_pref_data:    [*const u8;   RX_BURST],
    rx_pref_lens:    [u16;         RX_BURST],
    rx_pref_pos: u16,
    rx_pref_len: u16,
}

const RX_BURST: usize = 32;

enum DpdkRxToken {
    Mbuf { handle: *mut c_void, data: *const u8, len: usize },
    Synth(Vec<u8>),
}

struct DpdkTxToken<'a> {
    dev: &'a mut DpdkDevice,
}

impl RxToken for DpdkRxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        let mut this = core::mem::ManuallyDrop::new(self);
        match &mut *this {
            DpdkRxToken::Mbuf { handle, data, len } => {
                let slice = unsafe { core::slice::from_raw_parts(*data, *len) };
                let r = f(slice);
                unsafe { shim_mbuf_free(*handle) };
                r
            }
            DpdkRxToken::Synth(buf) => f(&buf[..]),
        }
    }
}

impl Drop for DpdkRxToken {
    fn drop(&mut self) {
        if let DpdkRxToken::Mbuf { handle, .. } = *self {
            unsafe { shim_mbuf_free(handle) };
        }
    }
}

impl<'a> TxToken for DpdkTxToken<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut handle: *mut c_void = ptr::null_mut();
        let mut cap: u16 = 0;
        let data = unsafe { shim_mbuf_alloc_tx(self.dev.pool, self.dev.queue_id, &mut handle, &mut cap) };
        if data.is_null() || handle.is_null() {
            // Pool exhausted. smoltcp still wants `f` called, so the frame is
            // discarded — but count it: dropping a SYN silently here looks
            // exactly like the peer never answering.
            TX_ALLOC_FAIL.fetch_add(1, Ordering::Relaxed);
            let mut scratch = [0u8; MTU];
            let n = core::cmp::min(len, scratch.len());
            return f(&mut scratch[..n]);
        }
        let n = core::cmp::min(len, cap as usize);
        let slice = unsafe { core::slice::from_raw_parts_mut(data, n) };
        let r = f(slice);
        if unsafe { shim_mbuf_tx(0, self.dev.queue_id, handle, n as u16) } != 0 {
            TX_BURST_FAIL.fetch_add(1, Ordering::Relaxed);
        }
        r
    }
}

impl Device for DpdkDevice {
    type RxToken<'a> = DpdkRxToken where Self: 'a;
    type TxToken<'a> = DpdkTxToken<'a> where Self: 'a;

    fn receive(&mut self, _t: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if let Some(buf) = self.pending_synth.take() {
            return Some((DpdkRxToken::Synth(buf), DpdkTxToken { dev: self }));
        }
        loop {
            // Refill the prefetch buffer if empty.
            if self.rx_pref_pos == self.rx_pref_len {
                let got = unsafe {
                    shim_mbuf_rx_burst_n(
                        0, self.queue_id,
                        self.rx_pref_handles.as_mut_ptr(),
                        self.rx_pref_data.as_mut_ptr(),
                        self.rx_pref_lens.as_mut_ptr(),
                        RX_BURST as u16,
                    )
                };
                if got == 0 { return None; }
                self.rx_pref_pos = 0;
                self.rx_pref_len = got;
            }
            let i = self.rx_pref_pos as usize;
            self.rx_pref_pos += 1;
            let handle = self.rx_pref_handles[i];
            let data   = self.rx_pref_data[i];
            let len    = self.rx_pref_lens[i];
            // Ethernet(14) + IPv4(20 min) + TCP(20 min) = 54; anything
            // shorter can't be a TCP flow we care about — pass it up.
            let ok = if (len as usize) < 54 {
                true
            } else {
                let bytes = unsafe { core::slice::from_raw_parts(data, len as usize) };
                let is_ipv4 = bytes[12] == 0x08 && bytes[13] == 0x00;
                if !is_ipv4 {
                    true
                } else {
                    let ihl = (bytes[14] & 0x0f) as usize * 4;
                    let proto = bytes[23];
                    let l4 = 14 + ihl;
                    if proto != 6 /* TCP */ || l4 + 4 > len as usize {
                        true
                    } else {
                        let dst_port = ((bytes[l4 + 2] as u16) << 8) | (bytes[l4 + 3] as u16);
                        let owned = self.owned_ports.is_null()
                            || owns_port(self.owned_ports, dst_port);
                        if !owned {
                            MISROUTED_DROPS.fetch_add(1, Ordering::Relaxed);
                        }
                        owned
                    }
                }
            };
            if ok {
                return Some((
                    DpdkRxToken::Mbuf { handle, data, len: len as usize },
                    DpdkTxToken { dev: self },
                ));
            }
            unsafe { shim_mbuf_free(handle) };
        }
    }

    fn transmit(&mut self, _t: Instant) -> Option<Self::TxToken<'_>> {
        Some(DpdkTxToken { dev: self })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.max_transmission_unit = MTU;
        c.medium = Medium::Ethernet;
        // NIC handles L3/L4 checksums; don't pay the CPU cost twice.
        c.checksum = ChecksumCapabilities::ignored();
        c
    }
}

// ---------------------------------------------------------------------------
// ARP helpers. We do our own ARP outside smoltcp so per-worker ifaces on
// non-0 RSS queues don't need to see the reply.
// ---------------------------------------------------------------------------

fn arp_frame(dst_mac: [u8; 6], src_mac: [u8; 6], op: u16, sender_ip: [u8; 4], target_mac: [u8; 6], target_ip: [u8; 4]) -> [u8; 42] {
    let mut f = [0u8; 42];
    f[0..6].copy_from_slice(&dst_mac);
    f[6..12].copy_from_slice(&src_mac);
    f[12..14].copy_from_slice(&[0x08, 0x06]); // ARP ethertype
    f[14..16].copy_from_slice(&[0x00, 0x01]); // HTYPE
    f[16..18].copy_from_slice(&[0x08, 0x00]); // PTYPE
    f[18] = 6;
    f[19] = 4;
    f[20..22].copy_from_slice(&op.to_be_bytes());
    f[22..28].copy_from_slice(&src_mac);
    f[28..32].copy_from_slice(&sender_ip);
    f[32..38].copy_from_slice(&target_mac);
    f[38..42].copy_from_slice(&target_ip);
    f
}

fn build_arp_request(our_mac: [u8; 6], our_ip: [u8; 4], target_ip: [u8; 4]) -> [u8; 42] {
    arp_frame([0xff; 6], our_mac, 1, our_ip, [0; 6], target_ip)
}

fn build_arp_reply(sender_mac: [u8; 6], sender_ip: [u8; 4], target_mac: [u8; 6], target_ip: [u8; 4]) -> Vec<u8> {
    arp_frame(target_mac, sender_mac, 2, sender_ip, target_mac, target_ip).to_vec()
}

fn parse_arp_reply_from(frame: &[u8], expected_ip: [u8; 4]) -> Option<[u8; 6]> {
    if frame.len() < 42 || &frame[12..14] != &[0x08, 0x06] || &frame[20..22] != &[0x00, 0x02] {
        return None;
    }
    if &frame[28..32] != &expected_ip[..] {
        return None;
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&frame[22..28]);
    Some(mac)
}

// ---------------------------------------------------------------------------
// Same-region S3 fetch. `TARGET_IP` is a fixed IP for the bucket to skip
// DNS; `TARGET_SNI`/`TARGET_HOST` are just the bucket hostname.
// ---------------------------------------------------------------------------

// Resolved by `just setup` from the bucket endpoint and baked in. The guest
// has no resolver by design, so this has to be decided at build time; if it
// goes stale the run fails loudly with a SYN timeout rather than silently.
const TARGET_IP_OCTETS: [u8; 4] = parse_ipv4(option_env!("AWS_TARGET_IP"));
const TARGET_IP: Ipv4Address = Ipv4Address::new(
    TARGET_IP_OCTETS[0], TARGET_IP_OCTETS[1], TARGET_IP_OCTETS[2], TARGET_IP_OCTETS[3]);

// Catch a missing or malformed address at build time. The runtime guard below
// still exists for safety, but discovering this on a booted instance costs a
// VM launch to learn something the compiler already knew.
const _: () = assert!(
    !(TARGET_IP_OCTETS[0] == 0 && TARGET_IP_OCTETS[1] == 0
      && TARGET_IP_OCTETS[2] == 0 && TARGET_IP_OCTETS[3] == 0),
    "AWS_TARGET_IP is unset or malformed - run `just setup smoltcp-s3`"
);
const TARGET_PORT: u16 = if PLAIN_HTTP { 80 } else { 443 };
// Endpoint is baked in at build time from $AWS_BUCKET / $AWS_REGION
// (see `just setup`).
const TARGET_HOST: &str = concat!(
    env!("AWS_BUCKET", "AWS_BUCKET is not set — run `just setup`"),
    ".s3.",
    env!("AWS_REGION", "AWS_REGION is not set — run `just setup`"),
    ".amazonaws.com"
);
const TARGET_SNI: &str = TARGET_HOST;
const TARGET_PATH: &[u8] = b"/blob.bin";

fn build_range_request(buf: &mut [u8], start: u64, end_inclusive: u64) -> usize {
    let mut w = BufWriter::new(buf);
    let _ = write!(
        &mut w,
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: minidpdk-smoltcp/0.1\r\nRange: bytes={}-{}\r\nConnection: close\r\n\r\n",
        core::str::from_utf8(TARGET_PATH).unwrap_or("/"),
        TARGET_HOST, start, end_inclusive,
    );
    w.used
}

// Real monotonic clock: fake-tick clocks drift smoltcp's retransmit
// timers at high throughput.
struct MonoClock {
    epoch_ns: u64,
}
impl MonoClock {
    fn new() -> Self { Self { epoch_ns: unsafe { shim_time_ns() } } }
    fn elapsed_ns(&self) -> u64 { unsafe { shim_time_ns() }.saturating_sub(self.epoch_ns) }
    fn elapsed_ms(&self) -> i64 { (self.elapsed_ns() / 1_000_000) as i64 }
}

// Safety net against a hung run spinning forever.
const ITER_BUDGET: u64 = 20_000_000_000;

fn dhcp_acquire(
    iface: &mut Interface,
    dev: &mut DpdkDevice,
    sockets: &mut SocketSet<'_>,
    dhcp_handle: smoltcp::iface::SocketHandle,
    clk: &MonoClock,
) -> Option<(smoltcp::wire::Ipv4Cidr, Ipv4Address)> {
    println!("DHCP: requesting lease...");
    let mut iter: u64 = 0;
    loop {
        let now_ms = clk.elapsed_ms();
        iface.poll(Instant::from_millis(now_ms), dev, sockets);

        match sockets.get_mut::<dhcpv4::Socket>(dhcp_handle).poll() {
            Some(dhcpv4::Event::Configured(cfg)) => {
                let a = cfg.address;
                let o = a.address().octets();
                println!("DHCP: address {}.{}.{}.{}/{}", o[0], o[1], o[2], o[3], a.prefix_len());
                let router = cfg.router.unwrap_or(Ipv4Address::new(0, 0, 0, 0));
                let r = router.octets();
                println!("DHCP: gateway {}.{}.{}.{}", r[0], r[1], r[2], r[3]);
                iface.update_ip_addrs(|addrs| { let _ = addrs.push(IpCidr::Ipv4(a)); });
                if let Some(gw) = cfg.router {
                    let _ = iface.routes_mut().add_default_ipv4_route(gw);
                }
                return Some((a, router));
            }
            Some(dhcpv4::Event::Deconfigured) => println!("DHCP: deconfigured"),
            None => {}
        }
        iter = iter.wrapping_add(1);
        if iter > ITER_BUDGET / 10 {
            println!("DHCP: timeout after {} ms", now_ms);
            return None;
        }
    }
}

// ---------------------------------------------------------------------------
// TLS pump on rustls's unbuffered API: drain outgoing → tcp tx, tcp rx →
// incoming, advance rustls state machine, exit on peer FIN.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ShimTimeProvider;

impl TimeProvider for ShimTimeProvider {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(core::time::Duration::from_secs(unsafe {
            shim_time_seconds()
        })))
    }
}

fn make_client_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg = ClientConfig::builder_with_details(
        Arc::new(rustls_rustcrypto::provider()),
        Arc::new(ShimTimeProvider),
    )
    .with_safe_default_protocol_versions()
    .expect("rustls: default protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    Arc::new(cfg)
}

// Sized to hold pipelined records without reallocating during the download.
const TLS_BUF_CAP: usize = 256 * 1024;


// Per-connection state driven by one shared iface.poll loop. Each
// connection carries its own TLS session, its own preformatted GET (with
// its own Range: header), and its own byte-tally. The main loop rotates
// through all `Conn`s each iteration until every one hits FIN.
struct Conn {
    handle: smoltcp::iface::SocketHandle,
    /// None under `PLAIN_HTTP`: no session to drive.
    tls: Option<UnbufferedClientConnection>,
    incoming: Vec<u8>,
    outgoing: Vec<u8>,
    request: Vec<u8>,
    request_queued: bool,
    handshake_done: bool,
    bytes_received: usize,
    done: bool,
    connect_start_ms: i64,
    // Identity, for diagnostics that need to name the failing flow.
    queue_id: u16,
    src_port: u16,
    // Plaintext bytes this connection's Range request should yield. The
    // completeness check compares against this rather than assuming every
    // connection succeeded.
    expected: u64,
    // Set when the peer closed after the response was fully requested, as
    // opposed to the connection being abandoned.
    closed_cleanly: bool,
    // HTTP response header skipping, so `bytes_received` is body-only.
    headers_done: bool,
    hdr_state: u8,
    status: u16,
    status_pos: u8,
    // --- instrumentation ---
    // Total SYNs sent, and whether this connection has left SynSent yet
    // (either established or given up).
    attempts: u16,
    settled: bool,
    start_ms: i64,
}

// A SYN now only goes unanswered on genuine loss, so the timeout is a
// backstop rather than a polling interval.
const SYN_TIMEOUT_MS: i64 = 5_000;

// Test knob: abandon this many connections per worker on purpose. A
// completeness check that has never been seen to fail is not a check, so
// setting this to 1 must turn a passing run into an INCOMPLETE one.
const FAULT_ABANDON_CONNS: usize = 0;

/// Count response *body* bytes only. The HTTP header is application data as
/// far as TLS is concerned, so counting raw decrypted length overcounts by one
/// header per connection and makes a byte-exact completeness check impossible.
/// The CRLFCRLF terminator can straddle records, hence the carried state.
fn count_body(bytes: &mut usize, headers_done: &mut bool, hdr_state: &mut u8,
              status: &mut u16, status_pos: &mut u8, data: &[u8]) {
    if *headers_done {
        *bytes += data.len();
        return;
    }
    for (i, &b) in data.iter().enumerate() {
        // "HTTP/1.1 206 ..." — the code is bytes 9..12. An error body counts
        // as bytes, so a refusal otherwise looks like a shortfall.
        if *status_pos < 12 {
            if *status_pos >= 9 && b.is_ascii_digit() {
                *status = *status * 10 + (b - b'0') as u16;
            }
            *status_pos += 1;
        }
        *hdr_state = match (*hdr_state, b) {
            (0, b'\r') => 1,
            (1, b'\n') => 2,
            (2, b'\r') => 3,
            (3, b'\n') => 4,
            (_, b'\r') => 1,
            _ => 0,
        };
        if *hdr_state == 4 {
            *headers_done = true;
            // The request is ranged, so only 206 is what we asked for.
            if *status != 206 {
                BAD_STATUS.fetch_add(1, Ordering::Relaxed);
                FIRST_BAD_STATUS.compare_exchange(0, *status as u64,
                    Ordering::Relaxed, Ordering::Relaxed).ok();
            }
            *bytes += data.len() - (i + 1);
            return;
        }
    }
}

// Drive one connection forward by one step (RX drain, TX push, TLS state
// machine). Returns `true` if the connection has terminated for any
// reason (clean FIN, SYN timeout, TLS error).
fn conn_step(
    conn: &mut Conn,
    sockets: &mut SocketSet<'_>,
    clk: &MonoClock,
) -> bool {
    if conn.done { return true; }
    let s = sockets.get_mut::<tcp::Socket>(conn.handle);
    let state = s.state();
    let now_ms = clk.elapsed_ms();
    // Short SYN-timeout: with N queues, only 1/N of return SYN-ACKs
    // land on our queue, so a failed try means "wrong port hash" not
    // "network issue". Server RTT is < 1 ms in-region, so 200 ms is
    // still 100× the round-trip.
    // Instrumentation: the moment a connection leaves SynSent for Established
    // is the moment its SYN-ACK finally came back on the right queue. Record
    // how many SYNs that took and how long it burned.
    if !conn.settled && state == tcp::State::Established {
        conn.settled = true;
        SYN_RETRIES.fetch_add(conn.attempts.saturating_sub(1) as u64, Ordering::Relaxed);
        SETUP_MS_TOTAL.fetch_add((now_ms - conn.start_ms).max(0) as u64, Ordering::Relaxed);
        CONNS_ESTABLISHED.fetch_add(1, Ordering::Relaxed);
    }

    // Source ports are now chosen so the return flow provably hashes to this
    // worker's queue, so a SYN that goes unanswered is a real failure — a lost
    // packet or an unreachable peer — not the "wrong port hash, roll again"
    // case the old retry loop existed to paper over. Give it a generous
    // window and then fail loudly rather than silently abandoning the range.
    if state == tcp::State::SynSent && now_ms - conn.connect_start_ms > SYN_TIMEOUT_MS {
        s.abort();
        println!("FAIL: q{} SYN timeout on port {} after {} ms — no SYN-ACK",
            conn.queue_id, conn.src_port, SYN_TIMEOUT_MS);
        if !conn.settled {
            conn.settled = true;
            CONNS_FAILED.fetch_add(1, Ordering::Relaxed);
            SETUP_MS_TOTAL.fetch_add((now_ms - conn.start_ms).max(0) as u64, Ordering::Relaxed);
        }
        conn.done = true;
        return true;
    }
    // No handshake to wait on, so the GET is queued as soon as the socket is
    // open. `handshake_done` then means "the request may go out", which is
    // what the clean-close check below reads it as.
    if PLAIN_HTTP && !conn.request_queued && s.may_send() {
        conn.outgoing.extend_from_slice(&conn.request);
        conn.request_queued = true;
        conn.handshake_done = true;
    }
    if !conn.outgoing.is_empty() && s.can_send() {
        if let Ok(n) = s.send_slice(&conn.outgoing) {
            if n > 0 { conn.outgoing.drain(..n); }
        }
    }
    // Drain until the socket is empty, not once: smoltcp hands back the
    // largest *contiguous* slice of its ring, so a single call leaves the
    // remainder behind whenever the data has wrapped.
    while s.can_recv() {
        match s.recv(|buf| { conn.incoming.extend_from_slice(buf); (buf.len(), buf.len()) }) {
            Ok(n) if n > 0 => continue,
            _ => break,
        }
    }

    if STUB_TLS_AFTER_HANDSHAKE && conn.handshake_done && conn.request_queued {
        conn.bytes_received += conn.incoming.len();
        conn.incoming.clear();
    }

    // With no record layer the socket already holds body bytes.
    if PLAIN_HTTP && !conn.incoming.is_empty() {
        count_body(&mut conn.bytes_received, &mut conn.headers_done,
            &mut conn.hdr_state, &mut conn.status, &mut conn.status_pos,
            &conn.incoming);
        conn.incoming.clear();
    }

    let mut progress = !PLAIN_HTTP;
    while progress {
        progress = false;
        let UnbufferedStatus { discard, state: tls_state } = match conn.tls.as_mut() {
            Some(tls) => tls.process_tls_records(&mut conn.incoming),
            None => break,
        };
        let st = match tls_state {
            Ok(st) => st,
            Err(e) => { println!("FAIL: tls: {:?}", e); conn.done = true; break; }
        };
        match st {
            ConnectionState::ReadTraffic(mut rt) => {
                while let Some(rec) = rt.next_record() {
                    match rec {
                        Ok(rec) => count_body(&mut conn.bytes_received,
                            &mut conn.headers_done,
                            &mut conn.hdr_state,
                            &mut conn.status,
                            &mut conn.status_pos, rec.payload),
                        Err(e) => { println!("FAIL: tls record: {:?}", e); conn.done = true; break; }
                    }
                }
                progress = true;
            }
            ConnectionState::EncodeTlsData(mut et) => {
                let head = conn.outgoing.len();
                conn.outgoing.resize(TLS_BUF_CAP, 0);
                match et.encode(&mut conn.outgoing[head..]) {
                    Ok(n) => { conn.outgoing.truncate(head + n); progress = true; }
                    Err(e) => { println!("FAIL: tls encode: {:?}", e); conn.done = true; break; }
                }
            }
            ConnectionState::TransmitTlsData(tt) => { tt.done(); progress = true; }
            ConnectionState::WriteTraffic(mut wt) => {
                conn.handshake_done = true;
                if !conn.request_queued {
                    let head = conn.outgoing.len();
                    conn.outgoing.resize(head + conn.request.len() + 128, 0);
                    match wt.encrypt(&conn.request, &mut conn.outgoing[head..]) {
                        Ok(n) => {
                            conn.outgoing.truncate(head + n);
                            conn.request_queued = true;
                            progress = true;
                        }
                        Err(e) => { println!("FAIL: tls encrypt: {:?}", e); conn.done = true; break; }
                    }
                }
            }
            _ => {}
        }
        conn.incoming.drain(..discard);
    }

    // Re-read the state: `state` was sampled before this iteration drained the
    // socket and advanced TLS. Requiring the receive buffer to be empty as well
    // is what stops a connection being called complete while bytes are still
    // sitting in it — the peer's FIN can arrive with data still buffered, and
    // declaring done there silently drops the tail of the transfer.
    let ended = matches!(
        s.state(),
        tcp::State::Closed | tcp::State::CloseWait | tcp::State::TimeWait
    );
    if conn.handshake_done && conn.request_queued && ended
        && conn.outgoing.is_empty() && !s.can_recv() {
        conn.closed_cleanly = true;
        conn.done = true;
    }
    conn.done
}

// ---------------------------------------------------------------------------
// Worker thread. Each worker owns one RSS queue and drives M parallel
// TLS connections through one iface.poll() loop; each connection fetches
// a disjoint byte range so the concurrent GETs together cover the
// worker's slice of the file.
// ---------------------------------------------------------------------------


#[repr(C)]
struct WorkerCtx {
    queue_id: u16,
    pool: *mut rte_pktmbuf_pool,
    mac: [u8; 6],
    ip: [u8; 4],
    prefix_len: u8,
    gateway_ip: [u8; 4],
    gateway_mac: [u8; 6],
    /// Index of this worker's first block in the global block sequence.
    first_block: u64,
    bytes_received: AtomicU64,
    elapsed_ns: AtomicU64,
    // Completeness accounting: what this worker's connections were asked to
    // fetch, and how many finished rather than being abandoned.
    bytes_expected: AtomicU64,
    conns_total: AtomicU64,
    conns_clean: AtomicU64,
}
unsafe impl Send for WorkerCtx {}
unsafe impl Sync for WorkerCtx {}

extern "C" fn worker_thread(arg: *mut c_void) {
    let ctx: &WorkerCtx = unsafe { &*(arg as *const WorkerCtx) };
    let clk = MonoClock::new();
    let ip = Ipv4Address::new(ctx.ip[0], ctx.ip[1], ctx.ip[2], ctx.ip[3]);
    let gw = Ipv4Address::new(ctx.gateway_ip[0], ctx.gateway_ip[1], ctx.gateway_ip[2], ctx.gateway_ip[3]);

    // Work out which source ports steer back to this worker's queue. The
    // bitmap is leaked so its pointer stays valid for the thread's lifetime;
    // workers never return.
    let owned: &'static mut [u8] =
        Box::leak(alloc::vec![0u8; OWNED_BITMAP_BYTES].into_boxed_slice());
    let owned_count = build_owned_ports(ctx.ip, ctx.queue_id, owned);
    let owned_ptr: *const u8 = owned.as_ptr();

    let mut dev = DpdkDevice {
        queue_id: ctx.queue_id,
        pool: ctx.pool,
        pending_synth: Some(build_arp_reply(ctx.gateway_mac, gw.octets(), ctx.mac, ip.octets())),
        owned_ports: owned_ptr,
        rx_pref_handles: [ptr::null_mut(); RX_BURST],
        rx_pref_data:    [ptr::null();     RX_BURST],
        rx_pref_lens:    [0;               RX_BURST],
        rx_pref_pos: 0,
        rx_pref_len: 0,
    };
    let config = Config::new(EthernetAddress(ctx.mac).into());
    let mut iface = Interface::new(config, &mut dev, Instant::from_millis(clk.elapsed_ms()));
    iface.update_ip_addrs(|addrs| { let _ = addrs.push(IpCidr::new(ip.into(), ctx.prefix_len)); });
    let _ = iface.routes_mut().add_default_ipv4_route(gw);

    // One SocketSet holding M TCP sockets. Buffers are leaked so their
    // 'static lifetime satisfies SocketSet's borrow.
    let mut storage: Vec<SocketStorage<'static>> =
        (0..CONNS_PER_WORKER).map(|_| SocketStorage::EMPTY).collect();
    let mut sockets: SocketSet<'static> = SocketSet::new(unsafe {
        // extend storage's lifetime to 'static — it lives for the rest
        // of this thread and workers never return
        core::mem::transmute::<&mut [SocketStorage<'_>], &mut [SocketStorage<'static>]>(
            &mut storage[..]
        )
    });
    let mut handles: Vec<smoltcp::iface::SocketHandle> = Vec::with_capacity(CONNS_PER_WORKER);
    for _ in 0..CONNS_PER_WORKER {
        let rx: &'static mut [u8] =
            Box::leak(alloc::vec![0u8; 4 * 1024 * 1024].into_boxed_slice());
        let tx: &'static mut [u8] =
            Box::leak(alloc::vec![0u8; 32 * 1024].into_boxed_slice());
        let mut sock = tcp::Socket::new(tcp::SocketBuffer::new(rx), tcp::SocketBuffer::new(tx));
        sock.set_ack_delay(None);
        handles.push(sockets.add(sock));
    }

    let cfg = make_client_config();
    let server_name = match ServerName::try_from(TARGET_SNI) {
        Ok(n) => n.to_owned(),
        Err(_) => { println!("FAIL: invalid ServerName"); return; }
    };

    // Split the worker's byte range across the connections it can actually
    // open. Computed after port selection so a worker short of usable ports
    // still covers its whole range rather than silently dropping the tail.


    // Collect this worker's owned ports, spread across the range rather than
    // taken consecutively — adjacent ports are no more likely to collide, but
    // spreading keeps the choice independent of any local structure in the
    // hash.
    let mut my_ports: Vec<u16> = Vec::with_capacity(CONNS_PER_WORKER);
    {
        let stride = core::cmp::max(1, (owned_count as usize) / CONNS_PER_WORKER);
        let mut seen = 0usize;
        for i in 0..EPH_LEN {
            if my_ports.len() == CONNS_PER_WORKER { break; }
            let port = EPH_BASE + i as u16;
            if owns_port(owned_ptr, port) {
                if seen % stride == 0 { my_ports.push(port); }
                seen += 1;
            }
        }
    }
    if my_ports.len() < CONNS_PER_WORKER {
        println!("q{}: only {} usable ports for {} connections",
            ctx.queue_id, my_ports.len(), CONNS_PER_WORKER);
    }
    println!("q{}: {} of {} ephemeral ports steer here; using {}",
        ctx.queue_id, owned_count, EPH_LEN, my_ports.len());

    let n_conns = core::cmp::min(CONNS_PER_WORKER, my_ports.len());

    let mut conns: Vec<Conn> = Vec::with_capacity(n_conns);
    // Ranges are split across all n_conns, but fault injection skips creating
    // the last few — leaving their byte ranges genuinely unrequested, which is
    // exactly the failure the completeness check has to catch.
    for i in 0..n_conns.saturating_sub(FAULT_ABANDON_CONNS) {
        // Every connection fetches one fixed-size block. Blocks are laid out
        // consecutively and wrap within the object, so distinct connections
        // read distinct offsets rather than replaying one hot range.
        let block = ctx.first_block + i as u64;
        let stride = OBJECT_SIZE.saturating_sub(BLOCK_SIZE) + 1;
        let start = if stride == 0 { 0 } else { block.wrapping_mul(BLOCK_SIZE) % stride };
        let end = start + BLOCK_SIZE - 1;
        // Chosen so the return flow provably hashes to this worker's queue.
        let src_port = my_ports[i];
        {
            let s = sockets.get_mut::<tcp::Socket>(handles[i]);
            if s.connect(iface.context(), (TARGET_IP, TARGET_PORT), src_port).is_err() {
                println!("q{}[{}] connect() rejected", ctx.queue_id, i);
                continue;
            }
        }
        let mut request_buf = [0u8; 384];
        let n = build_range_request(&mut request_buf, start, end);
        let tls = if PLAIN_HTTP {
            None
        } else {
            match UnbufferedClientConnection::new(cfg.clone(), server_name.clone()) {
                Ok(c) => Some(c),
                Err(e) => { println!("FAIL: rustls new: {:?}", e); return; }
            }
        };
        conns.push(Conn {
            handle: handles[i],
            tls,
            incoming: Vec::with_capacity(TLS_BUF_CAP),
            outgoing: Vec::with_capacity(TLS_BUF_CAP),
            request: request_buf[..n].to_vec(),
            request_queued: false,
            handshake_done: false,
            bytes_received: 0,
            done: false,
            connect_start_ms: clk.elapsed_ms(),
            queue_id: ctx.queue_id,
            src_port,
            expected: end - start + 1,
            closed_cleanly: false,
            headers_done: false,
            hdr_state: 0,
            status: 0,
            status_pos: 0,
            attempts: 1,
            settled: false,
            start_ms: clk.elapsed_ms(),
        });
    }

    let start_ns = clk.elapsed_ns();
    loop {
        let now_ms = clk.elapsed_ms();
        iface.poll(Instant::from_millis(now_ms), &mut dev, &mut sockets);
        let mut all_done = true;
        for c in conns.iter_mut() {
            let d = conn_step(c, &mut sockets, &clk);
            if !d { all_done = false; }
        }
        // Instrumentation: announce once, as soon as every connection has
        // either established or exhausted its retries. This is the point the
        // diagnostic summary can be printed — it does not wait for the
        // transfer, so it lands within seconds of boot.
        if all_done { break; }
    }
    let elapsed_ns = clk.elapsed_ns().saturating_sub(start_ns);
    let total: usize = conns.iter().map(|c| c.bytes_received).sum();
    ctx.bytes_received.store(total as u64, Ordering::Relaxed);
    ctx.elapsed_ns.store(elapsed_ns, Ordering::Relaxed);
    ctx.bytes_expected.store(conns.iter().map(|c| c.expected).sum(), Ordering::Relaxed);
    ctx.conns_total.store(conns.len() as u64, Ordering::Relaxed);
    ctx.conns_clean.store(
        conns.iter().filter(|c| c.closed_cleanly).count() as u64, Ordering::Relaxed);

    // A worker that could not open a connection for part of its range never
    // requested those bytes at all — surface it here, not as a silent gap.
    for c in conns.iter() {
        if !c.closed_cleanly {
            println!("q{}: conn on port {} did not close cleanly ({} of {} B)",
                ctx.queue_id, c.src_port, c.bytes_received, c.expected);
        }
    }
}


/// DHCP + gateway ARP on port 0 / queue 0. Workers inherit (ip, prefix, gw, gw_mac).
fn learn_network(
    pool: *mut rte_pktmbuf_pool,
    mac: [u8; 6],
) -> Option<(Ipv4Address, u8, Ipv4Address, EthernetAddress)> {
    let clk = MonoClock::new();
    // DHCP via smoltcp; scoped so its &mut dev is dropped before raw ARP.
    let (ip, prefix, gw) = {
        // DHCP / gateway-ARP path: accept every packet (len=0 disables).
        let mut dev = DpdkDevice {
            queue_id: 0, pool, pending_synth: None,
            owned_ports: ptr::null(),
            rx_pref_handles: [ptr::null_mut(); RX_BURST],
            rx_pref_data:    [ptr::null();     RX_BURST],
            rx_pref_lens:    [0;               RX_BURST],
            rx_pref_pos: 0,
            rx_pref_len: 0,
        };
        let config = Config::new(EthernetAddress(mac).into());
        let mut iface = Interface::new(config, &mut dev, Instant::from_millis(clk.elapsed_ms()));

        static mut STORAGE: [SocketStorage; 1] = [SocketStorage::EMPTY];
        let mut sockets = unsafe { SocketSet::new(&mut STORAGE[..]) };
        let dhcp_handle = sockets.add(dhcpv4::Socket::new());
        let (cidr, gw) = dhcp_acquire(&mut iface, &mut dev, &mut sockets, dhcp_handle, &clk)?;
        (cidr.address(), cidr.prefix_len(), gw)
    };

    // Raw ARP for the gateway. Bypasses smoltcp so worker ifaces on non-0
    // queues can be pre-seeded from the same reply.
    let req = build_arp_request(mac, ip.octets(), gw.octets());
    unsafe {
        let mut handle: *mut c_void = ptr::null_mut();
        let mut cap: u16 = 0;
        let data = shim_mbuf_alloc_tx(pool, 0, &mut handle, &mut cap);
        if data.is_null() || handle.is_null() {
            println!("FAIL: no mbuf for ARP");
            return None;
        }
        let n = core::cmp::min(req.len(), cap as usize);
        core::ptr::copy_nonoverlapping(req.as_ptr(), data, n);
        let _ = shim_mbuf_tx(0, 0, handle, n as u16);
    }

    let mut iter: u64 = 0;
    loop {
        let mut handle = [ptr::null_mut::<c_void>(); 1];
        let mut data = [ptr::null::<u8>(); 1];
        let mut len = [0u16; 1];
        let got = unsafe {
            shim_mbuf_rx_burst_n(0, 0, handle.as_mut_ptr(), data.as_mut_ptr(),
                                 len.as_mut_ptr(), 1)
        };
        if got == 1 {
            let slice = unsafe { core::slice::from_raw_parts(data[0], len[0] as usize) };
            let hw = parse_arp_reply_from(slice, gw.octets());
            unsafe { shim_mbuf_free(handle[0]) };
            if let Some(hw) = hw {
                println!("gateway MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    hw[0], hw[1], hw[2], hw[3], hw[4], hw[5]);
                return Some((ip, prefix, gw, EthernetAddress(hw)));
            }
        }
        iter = iter.wrapping_add(1);
        if iter > ITER_BUDGET / 20 {
            println!("FAIL: gateway ARP timed out");
            return None;
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn osv_app_main() {

    let t = TARGET_IP.octets();
    println!("bench: {} workers x {} conns x {} MiB block, tls_stub={} scheme={}",
        N_WORKERS_REQ, CONNS_PER_WORKER, BLOCK_SIZE / (1024 * 1024),
        STUB_TLS_AFTER_HANDSHAKE, if PLAIN_HTTP { "http" } else { "https" });
    println!("target: {}.{}.{}.{}:{} {}", t[0], t[1], t[2], t[3], TARGET_PORT, TARGET_HOST);
    if t == [0, 0, 0, 0] {
        println!("FAIL: AWS_TARGET_IP is unset or malformed — run `just setup smoltcp-s3`");
        exit();
    }
    if OBJECT_SIZE == 0 || BLOCK_SIZE == 0 || CONNS_PER_WORKER == 0 || N_WORKERS_REQ == 0 {
        println!("FAIL: BENCH_WORKERS, BENCH_CONNS_PER_WORKER, BENCH_BLOCK_SIZE and AWS_BUCKET_SIZE must be nonzero");
        exit();
    }

    // Clamp to what the device advertises. ENA VFs cap io-queue count
    // per instance size; rx_queue_setup for qid >= max is a hard reject.
    let mut max_rx: u16 = 0;
    let mut max_tx: u16 = 0;
    unsafe { shim_get_dev_info(0, &mut max_rx, &mut max_tx) };
    let dev_max = core::cmp::min(max_rx, max_tx);
    let n = if dev_max == 0 { N_WORKERS_REQ } else { core::cmp::min(N_WORKERS_REQ, dev_max) };
    if n != N_WORKERS_REQ {
        println!("clamping workers {} -> {} (device max)", N_WORKERS_REQ, n);
    }
    let n_queues: u16 = n;
    let n_workers: u64 = n as u64;

    let (pools, mac) = probe_and_open(n_queues).unwrap_or_else(|| {
        println!("FAIL: no usable NIC");
        exit();
    });

    if !load_rss_config(n_queues) {
        println!("FAIL: cannot predict RSS steering without the key and table");
        exit();
    }

    let (ip, prefix_len, gw, gw_mac) = learn_network(pools[0].0, mac).unwrap_or_else(|| {
        exit();
    });
    let ip_bytes = ip.octets();
    let gw_bytes = gw.octets();

    N_WORKERS.store(n_workers, Ordering::Relaxed);

    // No file split: every connection fetches one BLOCK_SIZE block, so the
    // bytes moved scale with worker count instead of being divided by it.
    let mut ctxs: Vec<Box<WorkerCtx>> = Vec::new();
    for i in 0..n_workers {

        // No port slabs: each worker derives its own ports from the RSS
        // hash, and the hash partitions the range disjointly by construction.
        let ctx = Box::new(WorkerCtx {
            queue_id: i as u16,
            pool: pools[i as usize].0,
            mac,
            ip: ip_bytes,
            prefix_len,
            gateway_ip: gw_bytes,
            gateway_mac: gw_mac.0,
            first_block: i * (CONNS_PER_WORKER as u64),
            bytes_received: AtomicU64::new(0),
            elapsed_ns: AtomicU64::new(0),
            bytes_expected: AtomicU64::new(0),
            conns_total: AtomicU64::new(0),
            conns_clean: AtomicU64::new(0),
        });
        ctxs.push(ctx);
    }

    let overall_clk = MonoClock::new();
    let handles: Vec<*mut c_void> = ctxs.iter().enumerate().map(|(i, ctx)| unsafe {
        shim_thread_spawn(worker_thread, (&**ctx as *const WorkerCtx) as *mut c_void, i as c_int)
    }).collect();
    for h in handles { unsafe { shim_thread_join(h) }; }
    let overall_ns = overall_clk.elapsed_ns();

    let mut total_b: u64 = 0;
    let mut total_expected: u64 = 0;
    let mut conns_total: u64 = 0;
    let mut conns_clean: u64 = 0;
    for (i, ctx) in ctxs.iter().enumerate() {
        let b = ctx.bytes_received.load(Ordering::Relaxed);
        let e = ctx.elapsed_ns.load(Ordering::Relaxed) as f64 / 1e9;
        total_b += b;
        total_expected += ctx.bytes_expected.load(Ordering::Relaxed);
        conns_total += ctx.conns_total.load(Ordering::Relaxed);
        conns_clean += ctx.conns_clean.load(Ordering::Relaxed);
        println!("worker {} (q{}): {} B / {:.3} s  ({:.1} MB/s)",
            i, i, b, e, (b as f64 / 1e6) / e.max(1e-9));
    }
    let overall_s = overall_ns as f64 / 1e9;
    println!();
    println!("AGGREGATE: {:.1} MiB in {:.3} s => {:.1} MB/s, {:.3} Gbps",
        total_b as f64 / (1024.0 * 1024.0),
        overall_s,
        total_b as f64 / 1e6 / overall_s.max(1e-9),
        total_b as f64 * 8.0 / 1e9 / overall_s.max(1e-9));

    // Completeness. Two things must hold, and they are separate questions:
    // every range was actually requested (a connection that never opened
    // abandons its range silently), and enough bytes arrived to cover them.
    let ranges_ok = conns_clean == conns_total && conns_total > 0;
    let planned_conns = n_workers * (CONNS_PER_WORKER as u64);
    let covered = conns_total == planned_conns;
    let misrouted = MISROUTED_DROPS.load(Ordering::Relaxed);
    let retries = SYN_RETRIES.load(Ordering::Relaxed);
    let failed = CONNS_FAILED.load(Ordering::Relaxed);
    let setup_ms = SETUP_MS_TOTAL.load(Ordering::Relaxed);

    println!();
    println!("connections   : {}/{} closed cleanly, {} failed", conns_clean, conns_total, failed);
    println!("syn retries   : {} (expected 0)", retries);
    println!("misrouted rx  : {} packets dropped (expected 0)", misrouted);
    let bad = BAD_STATUS.load(Ordering::Relaxed);
    let first_bad = FIRST_BAD_STATUS.load(Ordering::Relaxed);
    println!("http status   : {} non-206 responses (expected 0){}", bad,
        if bad > 0 {
            // Without naming it, throttling reads as lost bytes.
            if first_bad == 503 { " — 503 SlowDown, S3 is throttling" }
            else { " — see first code below" }
        } else { "" });
    if bad > 0 {
        println!("first bad code: {}", first_bad);
    }
    println!("tx drops      : {} no-mbuf, {} ring-full (expected 0)",
        TX_ALLOC_FAIL.load(Ordering::Relaxed), TX_BURST_FAIL.load(Ordering::Relaxed));

    // NIC counters. `imissed` is the one that matters for an unanswered SYN:
    // a SYN-ACK the device dropped for want of a descriptor never reaches any
    // queue, so from inside the stack it is indistinguishable from a peer that
    // never replied. Without this the cause cannot be attributed at all.
    let mut st = [0u64; 8];
    if unsafe { shim_eth_stats(0, st.as_mut_ptr(), st.len() as u16) } == 0 {
        println!("nic rx        : {} pkts, {} imissed, {} ierrors, {} nombuf",
            st[0], st[4], st[5], st[7]);
        println!("nic tx        : {} pkts, {} oerrors", st[1], st[6]);
        if st[4] > 0 || st[7] > 0 {
            println!("  ^ the NIC dropped frames before any queue saw them");
        }
    }
    let (mut qi, mut qe) = ([0u64; 32], [0u64; 32]);
    let nq = unsafe { shim_eth_qstats(0, qi.as_mut_ptr(), qe.as_mut_ptr(), n as u16) };
    if nq > 0 {
        for q in 0..(nq as usize).min(n as usize) {
            if qe[q] > 0 {
                println!("  q{}: {} rx pkts, {} errors", q, qi[q], qe[q]);
            }
        }
    }
    println!("setup         : {} ms total, {:.1} ms/conn",
        setup_ms, setup_ms as f64 / (conns_total.max(1)) as f64);
    println!("blocks        : {}/{} of {} MiB requested ({} bytes)",
        conns_total, planned_conns, BLOCK_SIZE / (1024 * 1024), total_expected);

    if STUB_TLS_AFTER_HANDSHAKE {
        // Ciphertext, so it carries TLS record and HTTP header overhead and
        // cannot be compared byte-for-byte against the plaintext ranges.
        let overhead = total_b as f64 - total_expected as f64;
        println!("bytes         : {} on the wire (ciphertext, {:+.2}%)",
            total_b, overhead * 100.0 / (total_expected.max(1)) as f64);
    } else {
        println!("bytes         : {} plaintext body", total_b);
    }

    let bytes_ok = if STUB_TLS_AFTER_HANDSHAKE {
        total_b >= total_expected
    } else {
        total_b == total_expected
    };

    if ranges_ok && covered && bytes_ok && misrouted == 0 {
        if STUB_TLS_AFTER_HANDSHAKE {
            println!("COMPLETE: all {} blocks fetched (set BENCH_TLS_STUB=0 for a byte-exact check)",
                conns_total);
        } else {
            println!("COMPLETE: {} bytes, byte-exact", total_b);
        }
    } else {
        println!("INCOMPLETE: {} connections abandoned, {} blocks unrequested, {} bytes {}",
            conns_total - conns_clean,
            planned_conns.saturating_sub(conns_total),
            if total_b > total_expected { total_b - total_expected } else { total_expected - total_b },
            if total_b > total_expected { "over" } else { "short" });
    }

    unsafe { shim_dev_stop(0) };
    exit();
}

fn exit() -> ! {
    loop { core::hint::spin_loop(); }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
