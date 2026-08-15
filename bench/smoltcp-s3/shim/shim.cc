// Implementation of the extern "C" bridge declared in shim.hh. See that
// file for the rationale: every real DPDK/minidpdk struct is built and
// read here, so only integers and opaque pointers ever cross into Rust.

#include "shim.hh"

#include <cstdarg>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <minidpdk/dev.hh>
#include <minidpdk/defs.hh>
#include <minidpdk/net.hh>
#include <minidpdk/rss.hh>
#include <osv/sched.hh>

namespace {

// Checksum offloads we ask ENA to do for us. TX asks the NIC to compute
// IPv4/TCP/UDP checksums; RX makes the NIC verify them. What each queue
// actually accepts is filled in below by shim_eth_dev_configure.
constexpr uint64_t kWantedTxOffloads =
    RTE_ETH_TX_OFFLOAD_IPV4_CKSUM |
    RTE_ETH_TX_OFFLOAD_TCP_CKSUM |
    RTE_ETH_TX_OFFLOAD_UDP_CKSUM;

constexpr uint64_t kWantedRxOffloads =
    RTE_ETH_RX_OFFLOAD_IPV4_CKSUM |
    RTE_ETH_RX_OFFLOAD_TCP_CKSUM |
    RTE_ETH_RX_OFFLOAD_UDP_CKSUM |
    // Ask the NIC to hand us the RSS hash it computed for each packet.
    // Besides being the ground truth for validating a software Toeplitz,
    // this bit also gates ena_rss_reta_query() and ena_rss_hash_conf_get()
    // — without it the guest cannot read back either the table or the key.
    RTE_ETH_RX_OFFLOAD_RSS_HASH;

uint64_t g_tx_offloads = 0;
uint64_t g_rx_offloads = 0;

rte_eth_dev *lookup_dev(uint16_t port_id) {
  return eth_os::get_eth_for_port(port_id);
}

}  // namespace

extern "C" {

int shim_is_valid_port(uint16_t port_id) {
  return lookup_dev(port_id) != nullptr ? 1 : 0;
}

int shim_get_dev_info(uint16_t port_id, uint16_t *max_rx_queues,
                       uint16_t *max_tx_queues) {
  rte_eth_dev *dev = lookup_dev(port_id);
  if (dev == nullptr) return -1;
  rte_eth_dev_info dev_info;
  std::memset(&dev_info, 0, sizeof(dev_info));
  dev->get_dev_info(&dev_info);
  *max_rx_queues = dev_info.max_rx_queues;
  *max_tx_queues = dev_info.max_tx_queues;
  return 0;
}

void *shim_pktmbuf_pool_create(const char *name, uint32_t n,
                                uint32_t cache_size, uint16_t priv_size,
                                uint16_t data_room_size) {
  return static_cast<void *>(
      rte_pktmbuf_pool_create(name, n, cache_size, priv_size, data_room_size,
                              SOCKET_ID_ANY));
}

void shim_mempool_free(void *pool) {
  if (pool) rte_mempool_free(static_cast<rte_mempool *>(pool));
}

int shim_eth_dev_configure(uint16_t port_id, uint16_t nb_rx_q,
                            uint16_t nb_tx_q) {
  auto *dev = lookup_dev(port_id);
  if (!dev) return -1;
  rte_eth_dev_info info;
  std::memset(&info, 0, sizeof(info));
  dev->get_dev_info(&info);
  // Only enable what the device advertises; smoltcp does the rest.
  g_tx_offloads = kWantedTxOffloads & info.tx_offload_capa;
  g_rx_offloads = kWantedRxOffloads & info.rx_offload_capa;

  rte_eth_conf conf;
  std::memset(&conf, 0, sizeof(conf));
  conf.txmode.offloads = g_tx_offloads;
  conf.rxmode.offloads = g_rx_offloads;
  // Multi-queue: RSS on the TCP/IPv4 4-tuple so parallel flows land on
  // distinct RX queues. Without this, all traffic goes to queue 0.
  if (nb_rx_q > 1) {
    conf.rxmode.mq_mode = RTE_ETH_MQ_RX_RSS;
    conf.rx_adv_conf.rss_conf.rss_hf = RTE_ETH_RSS_NONFRAG_IPV4_TCP;
  }
  return rte_eth_dev_configure(port_id, nb_rx_q, nb_tx_q, &conf);
}

void shim_adjust_nb_rx_tx_desc(uint16_t port_id, uint16_t *nb_rx_desc,
                                uint16_t *nb_tx_desc) {
  rte_eth_dev_adjust_nb_rx_tx_desc(port_id, nb_rx_desc, nb_tx_desc);
}

int shim_rx_queue_setup(uint16_t port_id, uint16_t queue_id,
                         uint16_t nb_desc, void *mempool) {
  rte_eth_rxconf rxconf;
  std::memset(&rxconf, 0, sizeof(rxconf));
  rxconf.offloads = g_rx_offloads;
  return rte_eth_rx_queue_setup(port_id, queue_id, nb_desc, SOCKET_ID_ANY,
                                 &rxconf, static_cast<rte_mempool *>(mempool));
}

int shim_tx_queue_setup(uint16_t port_id, uint16_t queue_id,
                         uint16_t nb_desc) {
  rte_eth_txconf txconf;
  std::memset(&txconf, 0, sizeof(txconf));
  txconf.offloads = g_tx_offloads;
  return rte_eth_tx_queue_setup(port_id, queue_id, nb_desc, SOCKET_ID_ANY,
                                 &txconf);
}

int shim_dev_start(uint16_t port_id) { return rte_eth_dev_start(port_id); }

void shim_dev_stop(uint16_t port_id) {
  rte_eth_dev *dev = lookup_dev(port_id);
  if (dev) dev->stop();
}

void shim_macaddr_get(uint16_t port_id, uint8_t *addr_bytes) {
  rte_ether_addr addr;
  std::memset(&addr, 0, sizeof(addr));
  rte_eth_macaddr_get(port_id, &addr);
  std::memcpy(addr_bytes, addr.addr.data(), RTE_ETHER_ADDR_LEN);
}

// Patch mbuf offload metadata + zero the cksum fields for the NIC to
// fill in. Applies only to IPv4 + (TCP|UDP) frames.
static void ena_tx_offload_prepare(rte_mbuf *m, uint8_t *buf, uint16_t len) {
  m->ol_flags = 0;
  m->l2_len = 0;
  m->l3_len = 0;
  m->l4_len = 0;
  if (g_tx_offloads == 0 || len < 14 + 20) return;
  const uint16_t ethertype = (uint16_t(buf[12]) << 8) | buf[13];
  if (ethertype != RTE_ETHER_TYPE_IPV4) return;
  uint8_t *ip = buf + 14;
  const uint16_t ip_hdr_len = uint16_t(ip[0] & 0x0f) * 4;
  if (ip_hdr_len < 20 || 14 + ip_hdr_len > len) return;

  m->l2_len = 14;
  m->l3_len = ip_hdr_len;
  m->ol_flags |= RTE_MBUF_F_TX_IPV4;
  if (g_tx_offloads & RTE_ETH_TX_OFFLOAD_IPV4_CKSUM) {
    m->ol_flags |= RTE_MBUF_F_TX_IP_CKSUM;
    ip[10] = 0; ip[11] = 0;
  }

  const uint8_t proto = ip[9];
  const uint16_t l4_off = 14 + ip_hdr_len;
  if (proto == 6 /* TCP */ && l4_off + 20 <= len &&
      (g_tx_offloads & RTE_ETH_TX_OFFLOAD_TCP_CKSUM)) {
    uint8_t *tcp = buf + l4_off;
    tcp[16] = 0; tcp[17] = 0;
    m->l4_len = uint16_t(tcp[12] >> 4) * 4;
    m->ol_flags |= RTE_MBUF_F_TX_TCP_CKSUM;
  } else if (proto == 17 /* UDP */ && l4_off + 8 <= len &&
             (g_tx_offloads & RTE_ETH_TX_OFFLOAD_UDP_CKSUM)) {
    uint8_t *udp = buf + l4_off;
    udp[6] = 0; udp[7] = 0;
    m->l4_len = 8;
    m->ol_flags |= RTE_MBUF_F_TX_UDP_CKSUM;
  }
}

uint8_t *shim_mbuf_alloc_tx(void *pool, uint16_t queue_id, void **out_handle,
                             uint16_t *out_cap) {
  (void)queue_id;
  rte_mbuf *m = rte_pktmbuf_alloc(static_cast<rte_mempool *>(pool));
  if (m == nullptr) {
    *out_handle = nullptr;
    *out_cap = 0;
    return nullptr;
  }
  *out_handle = m;
  *out_cap = m->buf_len;
  return rte_pktmbuf_mtod(m, uint8_t *);
}

int shim_mbuf_tx(uint16_t port_id, uint16_t queue_id, void *handle,
                  uint16_t len) {
  auto *m = static_cast<rte_mbuf *>(handle);
  m->data_len = len;
  m->pkt_len = len;
  m->nb_segs = 1;
  m->next = nullptr;
  ena_tx_offload_prepare(m, rte_pktmbuf_mtod(m, uint8_t *), len);
  if (rte_eth_tx_burst(port_id, queue_id, &m, 1) == 0) {
    rte_pktmbuf_free(m);
    return -1;
  }
  return 0;
}

void shim_mbuf_free(void *handle) {
  if (handle) rte_pktmbuf_free(static_cast<rte_mbuf *>(handle));
}

// The RSS hash the NIC computed for this packet, or 0 if the device did
// not tag it. Ground truth for validating a software Toeplitz.
uint32_t shim_mbuf_rss_hash(void *handle) {
  auto *m = static_cast<rte_mbuf *>(handle);
  if (m == nullptr || !(m->ol_flags & RTE_MBUF_F_RX_RSS_HASH)) return 0;
  return m->hash.rss;
}

// What the device actually accepted, after masking against rx_offload_capa.
uint64_t shim_rx_offloads(void) { return g_rx_offloads; }

// NIC-side counters. The only place a frame the hardware dropped before any
// queue saw it shows up — an unanswered SYN and a SYN-ACK the NIC discarded
// are indistinguishable from inside the stack without these.
// out: [ipackets, opackets, ibytes, obytes, imissed, ierrors, oerrors, rx_nombuf]
int shim_eth_stats(uint16_t port_id, uint64_t *out, uint16_t n) {
  rte_eth_dev *dev = lookup_dev(port_id);
  if (dev == nullptr || out == nullptr || n < 8) return -1;
  rte_eth_stats st;
  std::memset(&st, 0, sizeof(st));
  dev->get_stats(&st);
  out[0] = st.ipackets.load();
  out[1] = st.opackets.load();
  out[2] = st.ibytes.load();
  out[3] = st.obytes.load();
  out[4] = st.imissed.load();
  out[5] = st.ierrors.load();
  out[6] = st.oerrors.load();
  out[7] = st.rx_nombuf.load();
  return 0;
}

// Per-queue receive counters, so a drop can be attributed to the queue whose
// connection failed rather than only to the port as a whole.
int shim_eth_qstats(uint16_t port_id, uint64_t *ipkts, uint64_t *errs,
                    uint16_t nq) {
  rte_eth_dev *dev = lookup_dev(port_id);
  if (dev == nullptr || ipkts == nullptr || errs == nullptr) return -1;
  rte_eth_stats st;
  std::memset(&st, 0, sizeof(st));
  dev->get_stats(&st);
  uint16_t n = nq < RTE_ETHDEV_QUEUE_STAT_CNTRS ? nq : RTE_ETHDEV_QUEUE_STAT_CNTRS;
  for (uint16_t i = 0; i < n; i++) {
    ipkts[i] = st.q_ipackets[i];
    errs[i] = st.q_errors[i];
  }
  return n;
}

// --- RSS introspection -----------------------------------------------------
// The key the NIC is hashing with, and the indirection table it steers by.
// Both are read from the device/driver rather than assumed, so the port ->
// queue prediction can be built on measured values.

int shim_rss_hash_key(uint16_t port_id, uint8_t *out_key, uint16_t out_len) {
  rte_eth_dev *dev = lookup_dev(port_id);
  if (dev == nullptr || out_key == nullptr) return -1;

  rte_eth_dev_info info;
  std::memset(&info, 0, sizeof(info));
  dev->get_dev_info(&info);
  if (info.hash_key_size == 0 || out_len < info.hash_key_size) return -1;

  rte_eth_rss_conf conf;
  std::memset(&conf, 0, sizeof(conf));
  conf.rss_key = out_key;
  conf.rss_key_len = static_cast<uint8_t>(info.hash_key_size);
  const int rc = dev->rss_hash_conf_get(&conf);
  if (rc != 0) return rc;
  return info.hash_key_size;
}

// Number of indirection-table entries the device is using.
int shim_rss_reta_size(uint16_t port_id) {
  rte_eth_dev *dev = lookup_dev(port_id);
  if (dev == nullptr) return -1;
  rte_eth_dev_info info;
  std::memset(&info, 0, sizeof(info));
  dev->get_dev_info(&info);
  return static_cast<int>(info.reta_size);
}

// Copy the indirection table into `out` as one queue id per entry.
int shim_rss_reta(uint16_t port_id, uint16_t *out, uint16_t out_entries) {
  rte_eth_dev *dev = lookup_dev(port_id);
  if (dev == nullptr || out == nullptr) return -1;

  const int reta_size = shim_rss_reta_size(port_id);
  if (reta_size <= 0) return -1;
  uint16_t n = static_cast<uint16_t>(reta_size);
  if (n > out_entries) n = out_entries;

  // The query API works in groups of RTE_ETH_RETA_GROUP_SIZE entries, each
  // with a bitmask selecting which entries of the group to fill.
  const uint16_t groups =
      (n + RTE_ETH_RETA_GROUP_SIZE - 1) / RTE_ETH_RETA_GROUP_SIZE;
  if (groups > 8) return -1;  // 8 * 64 = 512 entries, the largest ENA table
  rte_eth_rss_reta_entry64 conf[8];
  std::memset(conf, 0, sizeof(conf));
  for (uint16_t g = 0; g < groups; g++) conf[g].mask = ~0ull;

  const int rc = dev->rss_reta_query(conf, n);
  if (rc != 0) return rc;

  for (uint16_t i = 0; i < n; i++) {
    out[i] = conf[i / RTE_ETH_RETA_GROUP_SIZE].reta[i % RTE_ETH_RETA_GROUP_SIZE];
  }
  return n;
}

// Batched RX: pulls up to `max` mbufs from the NIC in one call.
// The three output arrays are parallel; entries [0..returned) are
// filled with (mbuf handle, data pointer, data length). Bad-cksum
// packets are freed and skipped, so returned <= drained.
uint16_t shim_mbuf_rx_burst_n(uint16_t port_id, uint16_t queue_id,
                               void **out_handles, const uint8_t **out_data,
                               uint16_t *out_lens, uint16_t max) {
  rte_mbuf *bufs[32];
  if (max > 32) max = 32;
  const uint16_t got = rte_eth_rx_burst(port_id, queue_id, bufs, max);
  uint16_t n = 0;
  for (uint16_t i = 0; i < got; i++) {
    rte_mbuf *m = bufs[i];
    const uint64_t ol = m->ol_flags;
    const bool ip_bad =
        (ol & RTE_MBUF_F_RX_IP_CKSUM_MASK) == RTE_MBUF_F_RX_IP_CKSUM_BAD;
    const bool l4_bad =
        (ol & RTE_MBUF_F_RX_L4_CKSUM_MASK) == RTE_MBUF_F_RX_L4_CKSUM_BAD;
    if (ip_bad || l4_bad) {
      rte_pktmbuf_free(m);
      continue;
    }
    out_handles[n] = m;
    out_data[n]    = rte_pktmbuf_mtod(m, const uint8_t *);
    out_lens[n]    = m->data_len;
    n++;
  }
  return n;
}

uint64_t shim_time_seconds(void) {
  return static_cast<uint64_t>(std::time(nullptr));
}

uint64_t shim_time_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return static_cast<uint64_t>(ts.tv_sec) * 1000000000ull +
         static_cast<uint64_t>(ts.tv_nsec);
}

void *shim_thread_spawn(void (*fn)(void *), void *arg, int cpu_id) {
  sched::thread::attr attrs;
  if (cpu_id >= 0 && static_cast<size_t>(cpu_id) < sched::cpus.size()) {
    attrs.pin(sched::cpus[cpu_id]);
  }
  sched::thread *t =
      sched::thread::make([fn, arg]() { fn(arg); }, attrs);
  t->start();
  return static_cast<void *>(t);
}

void shim_thread_join(void *handle) {
  sched::thread *t = static_cast<sched::thread *>(handle);
  t->join();
  sched::thread::dispose(t);
}

void *shim_malloc(uint64_t size) {
  return std::malloc(static_cast<size_t>(size));
}

void shim_free(void *ptr) { std::free(ptr); }

void *shim_realloc(void *ptr, uint64_t size) {
  return std::realloc(ptr, static_cast<size_t>(size));
}

// The getrandom crate's non-custom path syscalls into SYS_getrandom.
// OSv has no Linux syscall table; fill from RDRAND instead. Only
// SYS_getrandom is recognized — anything else returns ENOSYS.
long syscall(long number, ...) {
  constexpr long SYS_getrandom = 318;
  if (number != SYS_getrandom) return -1;
  va_list ap;
  va_start(ap, number);
  void *buf = va_arg(ap, void *);
  size_t len = va_arg(ap, size_t);
  va_end(ap);
  auto *out = static_cast<uint8_t *>(buf);
  for (size_t i = 0; i < len; ) {
    uint64_t v;
    unsigned char ok = 0;
    for (int r = 0; r < 10 && !ok; ++r) {
      asm volatile("rdrand %0; setc %1" : "=r"(v), "=r"(ok));
    }
    if (!ok) return -1;
    size_t chunk = len - i < 8 ? len - i : 8;
    std::memcpy(out + i, &v, chunk);
    i += chunk;
  }
  return static_cast<long>(len);
}

}  // extern "C"
