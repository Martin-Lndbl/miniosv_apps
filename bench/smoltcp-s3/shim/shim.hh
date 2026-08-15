// extern "C" bridge between the Rust port and the C++-only minidpdk
// API. Every DPDK struct is built and read inside the shim; only
// integers and opaque pointers cross into Rust.
#pragma once
#include <cstdint>

extern "C" {

int shim_is_valid_port(uint16_t port_id);

int shim_get_dev_info(uint16_t port_id, uint16_t *max_rx_queues,
                       uint16_t *max_tx_queues);

void *shim_pktmbuf_pool_create(const char *name, uint32_t n,
                                uint32_t cache_size, uint16_t priv_size,
                                uint16_t data_room_size);
void shim_mempool_free(void *pool);

// If nb_rx_q > 1, enables RSS over the TCP/IPv4 4-tuple so parallel
// flows land on distinct RX queues.
int shim_eth_dev_configure(uint16_t port_id, uint16_t nb_rx_q,
                            uint16_t nb_tx_q);

void shim_adjust_nb_rx_tx_desc(uint16_t port_id, uint16_t *nb_rx_desc,
                                uint16_t *nb_tx_desc);

int shim_rx_queue_setup(uint16_t port_id, uint16_t queue_id,
                         uint16_t nb_desc, void *mempool);
int shim_tx_queue_setup(uint16_t port_id, uint16_t queue_id,
                         uint16_t nb_desc);

int shim_dev_start(uint16_t port_id);
void shim_dev_stop(uint16_t port_id);

void shim_macaddr_get(uint16_t port_id, uint8_t *addr_bytes);

// Zero-copy TX: allocate an mbuf from `pool`, expose its data area for
// direct write, return the mbuf handle. On success *out_handle is set
// and the returned pointer is the writable start; *out_cap is the max
// bytes that can be written. `queue_id` is used only for per-queue
// alloc-fail stats. Caller must eventually shim_mbuf_tx() or
// shim_mbuf_free() the handle.
uint8_t *shim_mbuf_alloc_tx(void *pool, uint16_t queue_id, void **out_handle,
                             uint16_t *out_cap);

// Enqueue a previously-allocated mbuf. Returns 0 on success (mbuf
// consumed by the NIC), -1 on tx failure (mbuf is freed).
int shim_mbuf_tx(uint16_t port_id, uint16_t queue_id, void *handle,
                  uint16_t len);
void shim_mbuf_free(void *handle);

// RSS hash the NIC computed for this packet (0 if untagged), and the RX
// offload mask the device actually accepted.
uint32_t shim_mbuf_rss_hash(void *handle);
uint64_t shim_rx_offloads(void);

// RSS introspection. `shim_rss_hash_key` writes the device's current
// Toeplitz key into out_key and returns its length in bytes (negative on
// failure). `shim_rss_reta` writes one queue id per indirection-table entry
// and returns the number written. Together they are everything needed to
// predict which RX queue a given 4-tuple will land on.
int shim_rss_hash_key(uint16_t port_id, uint8_t *out_key, uint16_t out_len);
int shim_rss_reta_size(uint16_t port_id);
int shim_rss_reta(uint16_t port_id, uint16_t *out, uint16_t out_entries);

// Zero-copy RX. Return 1 with (*out_handle, *out_data, *out_len) set;
// or 0 if no packet is available OR the NIC flagged a bad checksum
// (in which case the mbuf is freed internally).
int shim_mbuf_rx_burst(uint16_t port_id, uint16_t queue_id, void **out_handle,
                        const uint8_t **out_data, uint16_t *out_len);

// Batched RX: pulls up to `max` mbufs in one rte_eth_rx_burst call and
// writes them into the three parallel arrays. Returns the number of
// entries written (>= 0, <= max). Bad-cksum mbufs are freed inline.
uint16_t shim_mbuf_rx_burst_n(uint16_t port_id, uint16_t queue_id,
                               void **out_handles, const uint8_t **out_data,
                               uint16_t *out_lens, uint16_t max);

// OSv threading: pin `fn(arg)` to `cpu_id` (>=0), or leave unpinned if <0.
void *shim_thread_spawn(void (*fn)(void *), void *arg, int cpu_id);
void shim_thread_join(void *handle);

// Wall-clock seconds (for TLS cert validity) and monotonic ns
// (elapsed-time benchmarks).
uint64_t shim_time_seconds(void);
uint64_t shim_time_ns(void);

// Rust global allocator FFI.
void *shim_malloc(uint64_t size);
void  shim_free(void *ptr);
void *shim_realloc(void *ptr, uint64_t size);

}  // extern "C"
