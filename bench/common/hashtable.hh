#pragma once

// The workload for the PerfEvent comparison: an open-addressing hashtable,
// built and probed. Random access over a table far larger than L2, so it
// misses like the motivating example in the talk rather than like a loop that
// fits in registers.
//
// Header-only and free of libc beyond <cstdint>, because the same source is
// compiled by the miniOSv kernel toolchain and by musl-gcc for Linux. The
// table is a BSS array rather than a heap allocation so neither side is
// measuring its allocator.

#include <cstdint>
#include <cstddef>

namespace ht {

inline constexpr size_t bits = 21;          // 2M slots, 16 MiB
inline constexpr size_t size = 1ull << bits;
inline constexpr size_t mask = size - 1;

inline uint64_t slots[size];

// Zero the table. Must be called before each timed region and outside it:
// linear probing degenerates once the table fills, and without this the runs
// accumulate until a probe never terminates. It also makes both arms of a
// comparison do identical work.
inline void clear() {
  for (size_t i = 0; i < size; ++i)
    slots[i] = 0;
}

// Insert `n` keys, then look them all up. Returns a checksum so the loops
// cannot be elided.
inline uint64_t work(size_t n, uint64_t seed) {
  uint64_t x = seed, found = 0;

  for (size_t i = 0; i < n; ++i) {
    x = x * 6364136223846793005ull + 1442695040888963407ull;
    size_t h = (x >> 17) & mask;
    while (slots[h] && slots[h] != x)
      h = (h + 1) & mask;
    slots[h] = x;
  }

  x = seed;
  for (size_t i = 0; i < n; ++i) {
    x = x * 6364136223846793005ull + 1442695040888963407ull;
    size_t h = (x >> 17) & mask;
    while (slots[h] && slots[h] != x)
      h = (h + 1) & mask;
    found += slots[h] == x;
  }

  return found;
}

} // namespace ht
