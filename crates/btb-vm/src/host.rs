//! Host-call ABI: how a guest program talks to the chain.
//!
//! The guest executes `ecall` with the call number in a7 and arguments in a0..a5; the result (if
//! any) comes back in a0. `run_with_host` drives the VM and dispatches those calls to a `Host`.
//!
//! Every call charges gas BEFORE it runs (see `gas.rs`). If the charge fails the call has no
//! effect. Guest pointers and lengths are bounds-checked and trap, never panic. Addresses are 32
//! bytes; integers are little-endian u64 in registers.

use crate::decode::Trap;
use crate::gas;
use crate::vm::{Exit, Vm};

/// Stable host-call numbers (value of a7). Never renumber; only add.
pub mod abi {
    /// `storage_read(key_ptr, key_len, out_ptr, out_cap) -> value_len | NOT_FOUND`.
    /// Copies min(value_len, out_cap) bytes; returns the full length so the guest can retry.
    pub const STORAGE_READ: u64 = 1;
    /// `storage_write(key_ptr, key_len, val_ptr, val_len)`. Mutations only.
    pub const STORAGE_WRITE: u64 = 2;
    /// `storage_remove(key_ptr, key_len)`. Mutations only.
    pub const STORAGE_REMOVE: u64 = 3;
    /// `caller(out_ptr)`: writes the 32-byte caller address.
    pub const CALLER: u64 = 4;
    /// `block_height() -> u64`.
    pub const BLOCK_HEIGHT: u64 = 5;
    /// `token_balance(addr_ptr) -> u64` (balance of the 32-byte address).
    pub const TOKEN_BALANCE: u64 = 6;
    /// `token_transfer(to_ptr, amount) -> 0 ok | 1 insufficient`. Mutations only.
    pub const TOKEN_TRANSFER: u64 = 7;
    /// `return_data(ptr, len)`: ends execution with `Exit::Return`.
    pub const RETURN_DATA: u64 = 8;
    /// `abort(ptr, len)`: ends execution with `Trap::Abort(message)`.
    pub const ABORT: u64 = 9;

    /// Returned by `storage_read` when the key is missing.
    pub const NOT_FOUND: u64 = u64::MAX;
    /// `token_transfer` results.
    pub const TRANSFER_OK: u64 = 0;
    pub const TRANSFER_INSUFFICIENT: u64 = 1;
}

/// A 32-byte account address.
pub type Address = [u8; 32];

/// What the embedder (node runtime) provides to a running contract.
pub trait Host {
    /// True for queries: state-changing calls then trap with `Trap::WriteInQuery`.
    fn read_only(&self) -> bool {
        false
    }
    fn storage_read(&mut self, key: &[u8]) -> Option<Vec<u8>>;
    fn storage_write(&mut self, key: &[u8], value: &[u8]);
    fn storage_remove(&mut self, key: &[u8]);
    fn caller(&self) -> Address;
    fn block_height(&self) -> u64;
    fn token_balance(&self, addr: &Address) -> u64;
    /// Move `amount` from the contract to `to`; false (no change) if the balance is too low.
    fn token_transfer(&mut self, to: &Address, amount: u64) -> bool;
}

fn cost(base: u64, per_byte: u64, bytes: u64) -> u64 {
    base.saturating_add(per_byte.saturating_mul(bytes))
}

fn check_write(host: &impl Host) -> Result<(), Trap> {
    if host.read_only() {
        Err(Trap::WriteInQuery)
    } else {
        Ok(())
    }
}

fn addr(vm: &Vm, ptr: u64) -> Result<Address, Trap> {
    let mut a = [0u8; 32];
    a.copy_from_slice(vm.read_mem(ptr, 32)?);
    Ok(a)
}

/// Read a guest buffer; `len` is checked against memory size before any allocation.
fn buf(vm: &Vm, ptr: u64, len: u64) -> Result<Vec<u8>, Trap> {
    let len = usize::try_from(len).map_err(|_| Trap::MemoryOutOfBounds { addr: ptr })?;
    Ok(vm.read_mem(ptr, len)?.to_vec())
}

/// Run until the guest halts, returns data, or traps, serving host calls from `host`.
pub fn run_with_host(vm: &mut Vm, host: &mut impl Host) -> Result<Exit, Trap> {
    loop {
        match vm.run()? {
            Exit::Ecall => {}
            other => return Ok(other),
        }
        let [a0, a1, a2, a3] = [10, 11, 12, 13].map(|i| vm.reg(i));
        let ret = match vm.reg(17) {
            abi::STORAGE_READ => {
                vm.charge_gas(cost(gas::HOST_STORAGE_READ, gas::STORAGE_READ_BYTE, a1))?;
                let key = buf(vm, a0, a1)?;
                // Validate the output window before doing anything.
                buf(vm, a2, a3)?;
                match host.storage_read(&key) {
                    None => abi::NOT_FOUND,
                    Some(v) => {
                        let n = (v.len() as u64).min(a3);
                        vm.charge_gas(n.saturating_mul(gas::STORAGE_READ_BYTE))?;
                        vm.write_mem(a2, &v[..n as usize])?;
                        v.len() as u64
                    }
                }
            }
            abi::STORAGE_WRITE => {
                check_write(host)?;
                vm.charge_gas(cost(
                    gas::HOST_STORAGE_WRITE,
                    gas::STORAGE_WRITE_BYTE,
                    a1.saturating_add(a3),
                ))?;
                let (key, val) = (buf(vm, a0, a1)?, buf(vm, a2, a3)?);
                host.storage_write(&key, &val);
                0
            }
            abi::STORAGE_REMOVE => {
                check_write(host)?;
                vm.charge_gas(cost(gas::HOST_STORAGE_REMOVE, gas::STORAGE_REMOVE_BYTE, a1))?;
                let key = buf(vm, a0, a1)?;
                host.storage_remove(&key);
                0
            }
            abi::CALLER => {
                vm.charge_gas(gas::HOST_CALLER)?;
                vm.write_mem(a0, &host.caller())?;
                0
            }
            abi::BLOCK_HEIGHT => {
                vm.charge_gas(gas::HOST_BLOCK_HEIGHT)?;
                host.block_height()
            }
            abi::TOKEN_BALANCE => {
                vm.charge_gas(gas::HOST_TOKEN_BALANCE)?;
                host.token_balance(&addr(vm, a0)?)
            }
            abi::TOKEN_TRANSFER => {
                check_write(host)?;
                vm.charge_gas(gas::HOST_TOKEN_TRANSFER)?;
                let to = addr(vm, a0)?;
                if host.token_transfer(&to, a1) {
                    abi::TRANSFER_OK
                } else {
                    abi::TRANSFER_INSUFFICIENT
                }
            }
            abi::RETURN_DATA => {
                vm.charge_gas(cost(gas::HOST_RETURN_DATA, gas::RETURN_BYTE, a1))?;
                return Ok(Exit::Return(buf(vm, a0, a1)?));
            }
            abi::ABORT => {
                vm.charge_gas(cost(gas::HOST_ABORT, gas::ABORT_BYTE, a1))?;
                let msg = buf(vm, a0, a1)?;
                return Err(Trap::Abort(String::from_utf8_lossy(&msg).into_owned()));
            }
            n => return Err(Trap::UnknownHostCall(n)),
        };
        vm.set_reg(10, ret);
    }
}
