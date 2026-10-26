/* Example contract in C: a persistent counter. Build with ./build.sh. */
#include "btb.h"

static const uint8_t KEY[] = {'c', 'o', 'u', 'n', 't'};

static uint64_t load_u64(const uint8_t *p) {
  uint64_t v = 0;
  for (int i = 7; i >= 0; i--) v = (v << 8) | p[i];
  return v;
}

static void store_u64(uint8_t *p, uint64_t v) {
  for (int i = 0; i < 8; i++) p[i] = (uint8_t)(v >> (8 * i));
}

static uint64_t load(void) {
  uint8_t buf[8];
  if (btb_storage_read(KEY, sizeof KEY, buf, 8) != 8) return 0;
  return load_u64(buf);
}

/* Current value as u64 LE. */
BTB_QUERY(get) {
  (void)ptr;
  (void)len;
  uint8_t out[8];
  store_u64(out, load());
  btb_return_data(out, 8);
}

/* Argument: u64 LE (exactly 8 bytes). Returns the new total. */
BTB_MUTATION(increment) {
  if (len != 8) btb_abort("invalid arguments", 17);
  uint64_t by = load_u64(ptr);
  uint64_t cur = load();
  if (cur + by < cur) btb_abort("counter overflow", 16);
  uint8_t out[8];
  store_u64(out, cur + by);
  btb_storage_write(KEY, sizeof KEY, out, 8);
  btb_return_data(out, 8);
}

/* Cross-contract call from C. Argument: target[32] || kind u8 (0 query, 1 mutation) || function
 * name (rest). Calls the function with no arguments, forwarding all gas, and returns its output
 * (at most 64 bytes). A failed call aborts with the status's low byte as the message. */
BTB_MUTATION(relay) {
  if (len < 34) btb_abort("invalid arguments", 17);
  uint8_t req[160];
  uint8_t out[64];
  uint64_t n = btb_call_request(req, sizeof req, ptr, ptr[32], BTB_GAS_ALL,
                                (const char *)ptr + 33, (uint32_t)(len - 33), 0, 0);
  if (n == 0) btb_abort("request too large", 17);
  uint64_t r = btb_call_contract(req, n, out, sizeof out);
  if (r >= BTB_CALL_ERR_MIN) {
    uint8_t code = (uint8_t)(UINT64_MAX - r);
    btb_abort((const char *)&code, 1);
  }
  btb_return_data(out, r);
}
