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
use core::sync::atomic::{AtomicU64, Ordering};

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

struct Stdout;

impl Write for Stdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let mut off = 0;
        while off < bytes.len() {
            let n = unsafe { write(1, bytes[off..].as_ptr(), bytes.len() - off) };
            if n <= 0 {
                return Err(fmt::Error);
            }
            off += n as usize;
        }
        Ok(())
    }
}

macro_rules! println {
    () => {{ let _ = Stdout.write_str("\n"); }};
    ($($arg:tt)*) => {{
        let _ = Stdout.write_fmt(format_args!($($arg)*));
        let _ = Stdout.write_str("\n");
    }};
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
    fn shim_mbuf_rx_burst(
        port_id: u16,
        queue_id: u16,
        out_handle: *mut *mut c_void,
        out_data: *mut *const u8,
        out_len: *mut u16,
    ) -> c_int;
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
    const DESC_NUM: u16 = 1024;
    const CACHE: u32 = 64;
    // Each queue's RX ring pins DESC_NUM-1 mbufs; one ring's worth of
    // slack covers TX and in-flight app buffers.
    let per_queue_size: u32 = (DESC_NUM as u32) * 2;

    let mut pools: Vec<PktPool> = Vec::with_capacity(n_queues as usize);
    for q in 0..n_queues {
        let mut name = [0u8; 32];
        let _ = write!(&mut PoolName(&mut name), "bench-pool-{}\0", q);
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

// Tiny helper: format a null-terminated pool name into a fixed buffer
// without pulling in `alloc::format!`.
struct PoolName<'a>(&'a mut [u8]);
impl core::fmt::Write for PoolName<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = core::cmp::min(s.len(), self.0.len());
        self.0[..n].copy_from_slice(&s.as_bytes()[..n]);
        self.0 = &mut core::mem::take(&mut self.0)[n..];
        Ok(())
    }
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
    // Half-open port range [port_slab_start, port_slab_start+port_slab_len)
    // (modulo 2^16) of dst_ports this iface owns. RSS distributes return
    // SYN-ACKs and later packets by a hash the guest can't predict, so
    // some packets destined for a sibling worker's connection land on
    // this queue instead. If we let smoltcp see them, it demux-misses
    // and sends a RST — killing the sibling's handshake. Drop them
    // here silently. `port_slab_len = 0` means "accept all ports"
    // (used by the DHCP/ARP path in learn_network).
    port_slab_start: u16,
    port_slab_len: u16,
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
            // Pool exhausted; smoltcp still wants `f` called — discard.
            let mut scratch = [0u8; MTU];
            let n = core::cmp::min(len, scratch.len());
            return f(&mut scratch[..n]);
        }
        let n = core::cmp::min(len, cap as usize);
        let slice = unsafe { core::slice::from_raw_parts_mut(data, n) };
        let r = f(slice);
        let _ = unsafe { shim_mbuf_tx(0, self.dev.queue_id, handle, n as u16) };
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
                        self.port_slab_len == 0
                            || dst_port.wrapping_sub(self.port_slab_start) < self.port_slab_len
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

const TARGET_IP: Ipv4Address = Ipv4Address::new(3, 5, 216, 240);
const TARGET_PORT: u16 = 443;
const TARGET_SNI: &str = "miniosv-bench-1783870611.s3.eu-north-1.amazonaws.com";
const TARGET_HOST: &str = "miniosv-bench-1783870611.s3.eu-north-1.amazonaws.com";
const TARGET_PATH: &[u8] = b"/bench10.bin";

fn build_range_request(buf: &mut [u8], start: u64, end_inclusive: u64) -> usize {
    struct Wr<'a> { buf: &'a mut [u8], used: usize }
    impl core::fmt::Write for Wr<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let n = core::cmp::min(s.len(), self.buf.len() - self.used);
            self.buf[self.used..self.used + n].copy_from_slice(&s.as_bytes()[..n]);
            self.used += n;
            Ok(())
        }
    }
    let mut w = Wr { buf, used: 0 };
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

// Bench knob: once the handshake has completed and the GET is on the
// wire, discard every incoming byte without touching the rustls record
// layer. Isolates ENA + minidpdk + smoltcp cost from crypto cost.
const STUB_TLS_AFTER_HANDSHAKE: bool = true;

// Per-connection state driven by one shared iface.poll loop. Each
// connection carries its own TLS session, its own preformatted GET (with
// its own Range: header), and its own byte-tally. The main loop rotates
// through all `Conn`s each iteration until every one hits FIN.
struct Conn {
    handle: smoltcp::iface::SocketHandle,
    tls: UnbufferedClientConnection,
    tls_cfg: Arc<ClientConfig>,
    tls_server_name: ServerName<'static>,
    incoming: Vec<u8>,
    outgoing: Vec<u8>,
    request: Vec<u8>,
    request_queued: bool,
    handshake_done: bool,
    bytes_received: usize,
    done: bool,
    connect_start_ms: i64,
    // Reconnect scratch — when the SYN never gets ACKed the return
    // flow hashed to another worker's RSS queue. Bump the source port
    // and try again until the mapping happens to land on our queue.
    src_port_next: u16,
    retries_left: u8,
}

// Drive one connection forward by one step (RX drain, TX push, TLS state
// machine). Returns `true` if the connection has terminated for any
// reason (clean FIN, SYN timeout, TLS error).
fn conn_step(
    conn: &mut Conn,
    iface: &mut Interface,
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
    if state == tcp::State::SynSent && now_ms - conn.connect_start_ms > 200 {
        s.abort();
        if conn.retries_left == 0 {
            conn.done = true;
            return true;
        }
        conn.retries_left -= 1;
        let port = conn.src_port_next;
        conn.src_port_next = conn.src_port_next.wrapping_add(1);
        conn.incoming.clear();
        conn.outgoing.clear();
        conn.request_queued = false;
        conn.handshake_done = false;
        conn.connect_start_ms = now_ms;
        conn.bytes_received = 0;
        conn.tls = match UnbufferedClientConnection::new(
            conn.tls_cfg.clone(),
            conn.tls_server_name.clone(),
        ) {
            Ok(c) => c,
            Err(_) => { conn.done = true; return true; }
        };
        // The socket needs a fresh poll cycle before it will accept a
        // new connect(); calling it here works after abort().
        let _ = s.connect(iface.context(), (TARGET_IP, TARGET_PORT), port);
        return false;
    }
    if !conn.outgoing.is_empty() && s.can_send() {
        if let Ok(n) = s.send_slice(&conn.outgoing) {
            if n > 0 { conn.outgoing.drain(..n); }
        }
    }
    if s.can_recv() {
        let _ = s.recv(|buf| { conn.incoming.extend_from_slice(buf); (buf.len(), ()) });
    }

    if STUB_TLS_AFTER_HANDSHAKE && conn.handshake_done && conn.request_queued {
        conn.bytes_received += conn.incoming.len();
        conn.incoming.clear();
    }

    let mut progress = true;
    while progress {
        progress = false;
        let UnbufferedStatus { discard, state: tls_state } =
            conn.tls.process_tls_records(&mut conn.incoming);
        let st = match tls_state {
            Ok(st) => st,
            Err(e) => { println!("FAIL: tls: {:?}", e); conn.done = true; break; }
        };
        match st {
            ConnectionState::ReadTraffic(mut rt) => {
                while let Some(rec) = rt.next_record() {
                    match rec {
                        Ok(rec) => conn.bytes_received += rec.payload.len(),
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

    let ended = matches!(
        state,
        tcp::State::Closed | tcp::State::CloseWait | tcp::State::TimeWait
    );
    if conn.handshake_done && conn.request_queued && ended && conn.outgoing.is_empty() {
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

const CONNS_PER_WORKER: usize = 24;

#[repr(C)]
struct WorkerCtx {
    queue_id: u16,
    local_port: u16,
    port_slab: u16,
    pool: *mut rte_pktmbuf_pool,
    mac: [u8; 6],
    ip: [u8; 4],
    prefix_len: u8,
    gateway_ip: [u8; 4],
    gateway_mac: [u8; 6],
    range_start: u64,
    range_end_inclusive: u64,
    bytes_received: AtomicU64,
    elapsed_ns: AtomicU64,
}
unsafe impl Send for WorkerCtx {}
unsafe impl Sync for WorkerCtx {}

extern "C" fn worker_thread(arg: *mut c_void) {
    let ctx: &WorkerCtx = unsafe { &*(arg as *const WorkerCtx) };
    let clk = MonoClock::new();
    let ip = Ipv4Address::new(ctx.ip[0], ctx.ip[1], ctx.ip[2], ctx.ip[3]);
    let gw = Ipv4Address::new(ctx.gateway_ip[0], ctx.gateway_ip[1], ctx.gateway_ip[2], ctx.gateway_ip[3]);
    let mut dev = DpdkDevice {
        queue_id: ctx.queue_id,
        pool: ctx.pool,
        pending_synth: Some(build_arp_reply(ctx.gateway_mac, gw.octets(), ctx.mac, ip.octets())),
        port_slab_start: ctx.local_port,
        port_slab_len: ctx.port_slab,
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

    // Split the worker's byte range across M connections.
    let span = ctx.range_end_inclusive - ctx.range_start + 1;
    let per_conn = span / (CONNS_PER_WORKER as u64);

    let mut conns: Vec<Conn> = Vec::with_capacity(CONNS_PER_WORKER);
    for i in 0..CONNS_PER_WORKER {
        let start = ctx.range_start + (i as u64) * per_conn;
        let end = if i == CONNS_PER_WORKER - 1 {
            ctx.range_end_inclusive
        } else {
            start + per_conn - 1
        };
        // Give each connection a sub-slab within the worker's slab.
        // SYN-timeout retries bump the port by 1 and stay inside it.
        let per_conn_stride: u16 = ctx.port_slab / (CONNS_PER_WORKER as u16);
        let src_port = ctx.local_port.wrapping_add((i as u16) * per_conn_stride);
        {
            let s = sockets.get_mut::<tcp::Socket>(handles[i]);
            if s.connect(iface.context(), (TARGET_IP, TARGET_PORT), src_port).is_err() {
                println!("q{}[{}] connect() rejected", ctx.queue_id, i);
                continue;
            }
        }
        let mut request_buf = [0u8; 384];
        let n = build_range_request(&mut request_buf, start, end);
        let tls = match UnbufferedClientConnection::new(cfg.clone(), server_name.clone()) {
            Ok(c) => c,
            Err(e) => { println!("FAIL: rustls new: {:?}", e); return; }
        };
        conns.push(Conn {
            handle: handles[i],
            tls,
            tls_cfg: cfg.clone(),
            tls_server_name: server_name.clone(),
            incoming: Vec::with_capacity(TLS_BUF_CAP),
            outgoing: Vec::with_capacity(TLS_BUF_CAP),
            request: request_buf[..n].to_vec(),
            request_queued: false,
            handshake_done: false,
            bytes_received: 0,
            done: false,
            connect_start_ms: clk.elapsed_ms(),
            src_port_next: src_port.wrapping_add(1),
            retries_left: 32,
        });
    }

    let start_ns = clk.elapsed_ns();
    loop {
        let now_ms = clk.elapsed_ms();
        iface.poll(Instant::from_millis(now_ms), &mut dev, &mut sockets);
        let mut all_done = true;
        for c in conns.iter_mut() {
            let d = conn_step(c, &mut iface, &mut sockets, &clk);
            if !d { all_done = false; }
        }
        if all_done { break; }
    }
    let elapsed_ns = clk.elapsed_ns().saturating_sub(start_ns);
    let total: usize = conns.iter().map(|c| c.bytes_received).sum();
    ctx.bytes_received.store(total as u64, Ordering::Relaxed);
    ctx.elapsed_ns.store(elapsed_ns, Ordering::Relaxed);
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
            port_slab_start: 0, port_slab_len: 0,
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
        let mut handle: *mut c_void = ptr::null_mut();
        let mut data: *const u8 = ptr::null();
        let mut len: u16 = 0;
        let rc = unsafe { shim_mbuf_rx_burst(0, 0, &mut handle, &mut data, &mut len) };
        if rc == 1 && !handle.is_null() {
            let slice = unsafe { core::slice::from_raw_parts(data, len as usize) };
            let hw = parse_arp_reply_from(slice, gw.octets());
            unsafe { shim_mbuf_free(handle) };
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
    const N_REQ: u16 = 32;
    const FILE_SIZE: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB

    // Clamp to what the device advertises. ENA VFs cap io-queue count
    // per instance size; rx_queue_setup for qid >= max is a hard reject.
    let mut max_rx: u16 = 0;
    let mut max_tx: u16 = 0;
    unsafe { shim_get_dev_info(0, &mut max_rx, &mut max_tx) };
    let dev_max = core::cmp::min(max_rx, max_tx);
    let n = if dev_max == 0 { N_REQ } else { core::cmp::min(N_REQ, dev_max) };
    if n != N_REQ {
        println!("clamping workers {} -> {} (device max)", N_REQ, n);
    }
    let n_queues: u16 = n;
    let n_workers: u64 = n as u64;

    let (pools, mac) = probe_and_open(n_queues).unwrap_or_else(|| {
        println!("FAIL: no usable NIC");
        loop { core::hint::spin_loop(); }
    });

    let (ip, prefix_len, gw, gw_mac) = learn_network(pools[0].0, mac).unwrap_or_else(|| {
        loop { core::hint::spin_loop(); }
    });
    let ip_bytes = ip.octets();
    let gw_bytes = gw.octets();

    // Split the file across n_workers via HTTP Range so total bytes =
    // FILE_SIZE (not n_workers * FILE_SIZE).
    let chunk = FILE_SIZE / n_workers;
    let mut ctxs: Vec<Box<WorkerCtx>> = Vec::new();
    for i in 0..n_workers {
        let start = i * chunk;
        let end_inclusive = if i == n_workers - 1 { FILE_SIZE - 1 } else { start + chunk - 1 };
        // Split the ephemeral range 49152..65536 into n_workers disjoint slabs
        // so each worker's M connections (and their retries) can never
        // collide with another worker's 4-tuples.
        let port_slab: u16 = 16384 / n_queues;
        let local_port = 49152 + (i as u16) * port_slab;
        let ctx = Box::new(WorkerCtx {
            queue_id: i as u16,
            local_port,
            port_slab,
            pool: pools[i as usize].0,
            mac,
            ip: ip_bytes,
            prefix_len,
            gateway_ip: gw_bytes,
            gateway_mac: gw_mac.0,
            range_start: start,
            range_end_inclusive: end_inclusive,
            bytes_received: AtomicU64::new(0),
            elapsed_ns: AtomicU64::new(0),
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
    for (i, ctx) in ctxs.iter().enumerate() {
        let b = ctx.bytes_received.load(Ordering::Relaxed);
        let e = ctx.elapsed_ns.load(Ordering::Relaxed) as f64 / 1e9;
        total_b += b;
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

    unsafe { shim_dev_stop(0) };
    loop { core::hint::spin_loop(); }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
