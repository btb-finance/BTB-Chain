//! `table_index_scan` (call 15): request parsing, exact gas, copy semantics, default host.

use btb_vm::gas;
use btb_vm::host::{abi, parse_index_scan, Host, IndexBound, IndexKey, IndexScan, IndexScanPage};
use btb_vm::testing::MemHost;
use btb_vm::{run_with_host, Exit, Trap, Vm};

const ECALL: u32 = 0x0000_0073;
const EBREAK: u32 = 0x0010_0073;
const REQ: u64 = 256;
const OUT: u64 = 1024;

/// A host with a canned answer, to test the VM side of the call.
struct Canned {
    page: IndexScanPage,
    seen: Option<IndexScan>,
    read_only: bool,
}

impl Host for Canned {
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn storage_read(&mut self, _: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn storage_write(&mut self, _: &[u8], _: &[u8]) {}
    fn storage_remove(&mut self, _: &[u8]) {}
    fn caller(&self) -> [u8; 32] {
        [0; 32]
    }
    fn block_height(&self) -> u64 {
        0
    }
    fn token_balance(&self, _: &[u8; 32]) -> u64 {
        0
    }
    fn token_transfer(&mut self, _: &[u8; 32], _: u64) -> bool {
        false
    }
    fn table_insert(&mut self, _: &[u8], _: &[u8]) -> u64 {
        0
    }
    fn table_get(&mut self, _: &[u8], _: u64) -> Option<Vec<u8>> {
        None
    }
    fn table_update(&mut self, _: &[u8], _: u64, _: &[u8]) -> bool {
        false
    }
    fn table_delete(&mut self, _: &[u8], _: u64) -> bool {
        false
    }
    fn table_scan(&mut self, _: &[u8], _: u64, _: u64) -> Vec<u64> {
        Vec::new()
    }
    fn table_index_scan(&mut self, scan: &IndexScan) -> IndexScanPage {
        self.seen = Some(scan.clone());
        self.page.clone()
    }
}

fn put(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

/// table "Todo", index "owner", lo = Included(Bytes "ab"), hi = Unbounded, ascending.
fn request(limit: u32, cursor: &[u8]) -> Vec<u8> {
    let mut r = Vec::new();
    put(&mut r, b"Todo");
    put(&mut r, b"owner");
    r.extend_from_slice(&[1, 3]);
    put(&mut r, b"ab");
    r.push(0);
    r.push(0);
    r.extend_from_slice(&limit.to_le_bytes());
    put(&mut r, cursor);
    r
}

fn vm_with(req: &[u8], out_cap: u64) -> Vm {
    let mut vm = Vm::new(8192, 1_000_000);
    vm.write_mem(0, &ECALL.to_le_bytes()).unwrap();
    vm.write_mem(4, &EBREAK.to_le_bytes()).unwrap();
    vm.set_reg(17, abi::TABLE_INDEX_SCAN);
    for (i, a) in [REQ, req.len() as u64, OUT, out_cap].iter().enumerate() {
        vm.set_reg(10 + i, *a);
    }
    vm.write_mem(REQ, req).unwrap();
    vm
}

fn canned(ids: &[u64], cursor: &[u8]) -> Canned {
    Canned {
        page: IndexScanPage {
            ids: ids.to_vec(),
            next_cursor: cursor.to_vec(),
        },
        seen: None,
        read_only: false,
    }
}

#[test]
fn request_is_parsed_and_result_laid_out_with_exact_gas() {
    let req = request(10, b"cur");
    let mut h = canned(&[3, 7], b"next");
    let mut vm = vm_with(&req, 256);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    let seen = h.seen.unwrap();
    assert_eq!(seen.table, b"Todo");
    assert_eq!(seen.index, "owner");
    assert_eq!(
        seen.lo,
        IndexBound::Included(IndexKey::Bytes(b"ab".to_vec()))
    );
    assert_eq!(seen.hi, IndexBound::Unbounded);
    assert!(!seen.descending);
    assert_eq!((seen.limit, seen.cursor.as_slice()), (10, &b"cur"[..]));
    let total = 8 + 16 + 4;
    assert_eq!(vm.reg(10), total);
    let mut want = vec![2, 0, 0, 0, 4, 0, 0, 0];
    want.extend(3u64.to_le_bytes());
    want.extend(7u64.to_le_bytes());
    want.extend(b"next");
    assert_eq!(vm.read_mem(OUT, total as usize).unwrap(), want);
    // ECALL + EBREAK, base + request bytes, then 2 ids and 4 cursor bytes.
    let expect = 2
        + gas::HOST_TABLE_INDEX_SCAN
        + gas::TABLE_READ_BYTE * req.len() as u64
        + gas::TABLE_INDEX_SCAN_ID * 2
        + gas::TABLE_READ_BYTE * 4;
    assert_eq!(vm.gas_used(), expect);
}

#[test]
fn limit_is_capped_and_descending_flag_read() {
    let mut req = request(1_000_000, b"");
    // flags byte sits before limit (4) and cursor len (4).
    let n = req.len();
    req[n - 9] = 1;
    let mut h = canned(&[], b"");
    let mut vm = vm_with(&req, 64);
    run_with_host(&mut vm, &mut h).unwrap();
    let seen = h.seen.unwrap();
    assert!(seen.descending);
    assert_eq!(seen.limit, abi::MAX_INDEX_SCAN_LIMIT);
    assert_eq!(vm.reg(10), 8);
}

#[test]
fn too_small_buffer_returns_full_length_and_writes_nothing() {
    let req = request(10, b"");
    let mut h = canned(&[1, 2, 3], b"");
    let mut vm = vm_with(&req, 16);
    run_with_host(&mut vm, &mut h).unwrap();
    assert_eq!(vm.reg(10), 8 + 24);
    assert_eq!(vm.read_mem(OUT, 32).unwrap(), vec![0u8; 32]);
}

#[test]
fn allowed_in_read_only_mode() {
    let mut h = canned(&[1], b"");
    h.read_only = true;
    let mut vm = vm_with(&request(10, b""), 64);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(vm.reg(10), 16);
}

#[test]
fn default_host_has_no_indexes() {
    let mut h = MemHost::default();
    let mut vm = vm_with(&request(10, b""), 64);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(vm.reg(10), 8);
    assert_eq!(vm.read_mem(OUT, 8).unwrap(), vec![0u8; 8]);
}

#[test]
fn malformed_requests_trap() {
    let good = request(10, b"");
    let mut cases: Vec<Vec<u8>> = vec![Vec::new(), good[..good.len() - 1].to_vec()];
    let mut trailing = good.clone();
    trailing.push(0);
    cases.push(trailing);
    let mut bad_kind = good.clone();
    bad_kind[4 + 4 + 4 + 5] = 9; // lo kind
    cases.push(bad_kind);
    let mut bad_flags = good.clone();
    let n = bad_flags.len();
    bad_flags[n - 9] = 2;
    cases.push(bad_flags);
    for req in cases {
        assert!(parse_index_scan(&req).is_none());
        let mut vm = vm_with(&req, 64);
        let r = run_with_host(&mut vm, &mut MemHost::default());
        assert!(matches!(r, Err(Trap::Abort(_))), "{r:?}");
    }
    // Invalid UTF-8 index name, and an integer bound of the wrong width.
    let mut r = Vec::new();
    put(&mut r, b"T");
    put(&mut r, &[0xff]);
    r.extend_from_slice(&[0, 0, 0]);
    r.extend_from_slice(&1u32.to_le_bytes());
    put(&mut r, b"");
    assert!(parse_index_scan(&r).is_none());
    let mut r = Vec::new();
    put(&mut r, b"T");
    put(&mut r, b"i");
    r.extend_from_slice(&[1, 0]);
    put(&mut r, &[1, 2, 3]); // u64 needs 8 bytes
    assert!(parse_index_scan(&r).is_none());
}

#[test]
fn bad_guest_pointers_trap() {
    let mut vm = vm_with(&request(1, b""), 64);
    vm.set_reg(13, u64::MAX / 2); // out_cap beyond memory
    let r = run_with_host(&mut vm, &mut MemHost::default());
    assert!(matches!(r, Err(Trap::MemoryOutOfBounds { .. })), "{r:?}");
}

#[test]
fn out_of_gas_before_work() {
    let req = request(1, b"");
    let mut h = canned(&[1], b"");
    // Not enough for the base charge: the host is never asked.
    let mut tiny = Vm::new(8192, 100);
    tiny.write_mem(0, &ECALL.to_le_bytes()).unwrap();
    tiny.set_reg(17, abi::TABLE_INDEX_SCAN);
    tiny.set_reg(10, REQ);
    tiny.set_reg(11, req.len() as u64);
    tiny.set_reg(12, OUT);
    tiny.set_reg(13, 64);
    tiny.write_mem(REQ, &req).unwrap();
    assert_eq!(run_with_host(&mut tiny, &mut h), Err(Trap::OutOfGas));
    assert!(h.seen.is_none());
}
