/*
 * btb.h - host-call ABI for BTB Chain contracts written in C.
 *
 * Freestanding (needs only <stdint.h>). Target: RISC-V rv64im, lp64, no relaxation.
 * Call numbers and semantics mirror crates/btb-vm/src/host.rs (`abi`); see docs/LANGUAGE.md.
 * Never renumber; only add. Arguments and results use the encoding in docs/ENCODING.md
 * (little-endian integers, no padding).
 */
#ifndef BTB_H
#define BTB_H

#include <stdint.h>

/* Host-call numbers (value of a7). */
#define BTB_STORAGE_READ 1
#define BTB_STORAGE_WRITE 2
#define BTB_STORAGE_REMOVE 3
#define BTB_CALLER 4
#define BTB_BLOCK_HEIGHT 5
#define BTB_TOKEN_BALANCE 6
#define BTB_TOKEN_TRANSFER 7
#define BTB_RETURN_DATA 8
#define BTB_ABORT 9
#define BTB_TABLE_INSERT 10
#define BTB_TABLE_GET 11
#define BTB_TABLE_UPDATE 12
#define BTB_TABLE_DELETE 13
#define BTB_TABLE_SCAN 14
#define BTB_TABLE_INDEX_SCAN 15

/* Results. */
#define BTB_NOT_FOUND UINT64_MAX
#define BTB_TRANSFER_OK 0
#define BTB_TRANSFER_INSUFFICIENT 1
#define BTB_TABLE_OK 0
#define BTB_TABLE_MISSING 1

/* Entry points: btb_<kind>_<name>(input, input_len). Name is [a-z0-9_], 1..64 bytes. */
#define BTB_ENTRY(kind, name)                                                  \
  __attribute__((used, visibility("default"))) void btb_##kind##_##name(       \
      const uint8_t *ptr, uint64_t len)
#define BTB_QUERY(name) BTB_ENTRY(query, name)
#define BTB_MUTATION(name) BTB_ENTRY(mutation, name)
#define BTB_CRON(name) BTB_ENTRY(cron, name)
#define BTB_ACTION(name) BTB_ENTRY(action, name)

/* Raw ecalls. Memory clobber: the host reads or writes guest memory. */
static inline uint64_t btb_ecall5(uint64_t n, uint64_t a0, uint64_t a1,
                                  uint64_t a2, uint64_t a3, uint64_t a4) {
  register uint64_t r0 __asm__("a0") = a0;
  register uint64_t r1 __asm__("a1") = a1;
  register uint64_t r2 __asm__("a2") = a2;
  register uint64_t r3 __asm__("a3") = a3;
  register uint64_t r4 __asm__("a4") = a4;
  register uint64_t r7 __asm__("a7") = n;
  __asm__ volatile("ecall"
                   : "+r"(r0)
                   : "r"(r1), "r"(r2), "r"(r3), "r"(r4), "r"(r7)
                   : "memory");
  return r0;
}

#define BTB_P(x) ((uint64_t)(uintptr_t)(x))

/* Returns the full value length (may exceed out_cap) or BTB_NOT_FOUND. */
static inline uint64_t btb_storage_read(const uint8_t *key, uint64_t key_len,
                                        uint8_t *out, uint64_t out_cap) {
  return btb_ecall5(BTB_STORAGE_READ, BTB_P(key), key_len, BTB_P(out), out_cap, 0);
}
/* Mutations only. */
static inline void btb_storage_write(const uint8_t *key, uint64_t key_len,
                                     const uint8_t *val, uint64_t val_len) {
  btb_ecall5(BTB_STORAGE_WRITE, BTB_P(key), key_len, BTB_P(val), val_len, 0);
}
/* Mutations only. */
static inline void btb_storage_remove(const uint8_t *key, uint64_t key_len) {
  btb_ecall5(BTB_STORAGE_REMOVE, BTB_P(key), key_len, 0, 0, 0);
}
/* Writes the 32-byte caller address. */
static inline void btb_caller(uint8_t out[32]) {
  btb_ecall5(BTB_CALLER, BTB_P(out), 0, 0, 0, 0);
}
static inline uint64_t btb_block_height(void) {
  return btb_ecall5(BTB_BLOCK_HEIGHT, 0, 0, 0, 0, 0);
}
/* addr points to 32 bytes. */
static inline uint64_t btb_token_balance(const uint8_t addr[32]) {
  return btb_ecall5(BTB_TOKEN_BALANCE, BTB_P(addr), 0, 0, 0, 0);
}
/* Mutations only. Returns BTB_TRANSFER_OK or BTB_TRANSFER_INSUFFICIENT. */
static inline uint64_t btb_token_transfer(const uint8_t to[32], uint64_t amount) {
  return btb_ecall5(BTB_TOKEN_TRANSFER, BTB_P(to), amount, 0, 0, 0);
}
/* Ends execution, returning the bytes to the caller. */
static inline void btb_return_data(const uint8_t *ptr, uint64_t len) {
  btb_ecall5(BTB_RETURN_DATA, BTB_P(ptr), len, 0, 0, 0);
}
/* Ends execution with a trap carrying the message. Never returns. */
static inline __attribute__((noreturn)) void btb_abort(const char *msg, uint64_t len) {
  btb_ecall5(BTB_ABORT, BTB_P(msg), len, 0, 0, 0);
  for (;;) {
  }
}
/* Mutations only. Returns the new row id. */
static inline uint64_t btb_table_insert(const uint8_t *table, uint64_t table_len,
                                        const uint8_t *row, uint64_t row_len) {
  return btb_ecall5(BTB_TABLE_INSERT, BTB_P(table), table_len, BTB_P(row), row_len, 0);
}
/* Returns the full row length or BTB_NOT_FOUND. */
static inline uint64_t btb_table_get(const uint8_t *table, uint64_t table_len,
                                     uint64_t id, uint8_t *out, uint64_t out_cap) {
  return btb_ecall5(BTB_TABLE_GET, BTB_P(table), table_len, id, BTB_P(out), out_cap);
}
/* Mutations only. Returns BTB_TABLE_OK or BTB_TABLE_MISSING. */
static inline uint64_t btb_table_update(const uint8_t *table, uint64_t table_len,
                                        uint64_t id, const uint8_t *row, uint64_t row_len) {
  return btb_ecall5(BTB_TABLE_UPDATE, BTB_P(table), table_len, id, BTB_P(row), row_len);
}
/* Mutations only. Returns BTB_TABLE_OK or BTB_TABLE_MISSING. */
static inline uint64_t btb_table_delete(const uint8_t *table, uint64_t table_len,
                                        uint64_t id) {
  return btb_ecall5(BTB_TABLE_DELETE, BTB_P(table), table_len, id, 0, 0);
}
/* Writes up to out_cap/8 ascending row ids >= start_id as u64 LE; returns the count. */
static inline uint64_t btb_table_scan(const uint8_t *table, uint64_t table_len,
                                      uint64_t start_id, uint8_t *out, uint64_t out_cap) {
  return btb_ecall5(BTB_TABLE_SCAN, BTB_P(table), table_len, start_id, BTB_P(out), out_cap);
}
/* Read-only (allowed in queries). req is the request blob described in
 * crates/btb-vm/src/host.rs (abi::TABLE_INDEX_SCAN). Returns the full result length; the
 * result (u32 count, u32 cursor_len, count x u64 row ids, cursor; little-endian) is copied
 * only if it fits in out_cap. */
static inline uint64_t btb_table_index_scan(const uint8_t *req, uint64_t req_len,
                                            uint8_t *out, uint64_t out_cap) {
  return btb_ecall5(BTB_TABLE_INDEX_SCAN, BTB_P(req), req_len, BTB_P(out), out_cap, 0);
}

#endif /* BTB_H */
