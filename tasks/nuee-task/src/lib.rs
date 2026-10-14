//! Minimal SDK for Nuée WebAssembly tasks.
//!
//! A task module imports nothing and exports `memory`, `nuee_alloc` and `nuee_run`:
//! - `nuee_alloc(len: i32) -> i32` reserves `len` bytes and returns their address, or 0 when the
//!   allocation fails (the host reports `Oom`).
//! - `nuee_run(ptr: i32, len: i32) -> i64` runs the task on the `len` bytes at `ptr` and returns
//!   `(out_ptr << 32) | out_len`.
//!
//! The host creates a fresh instance per lease, calls `nuee_alloc`, copies the input, calls
//! `nuee_run` and reads the output. When the job lists assets, the input is an envelope
//! (`u32 LE count`, then `u32 LE length` + bytes per asset, then the lease input) that
//! [`split_assets`] takes apart. A panic aborts the module, which the host sees as a trap.
#![no_std]

extern crate alloc;

use alloc::vec::Vec;

/// Maximum number of assets accepted in an envelope.
pub const MAX_ASSETS: usize = 16;

/// Splits an asset envelope into the assets (in manifest order) and the lease input.
///
/// Returns `None` when the envelope is truncated or lists more than [`MAX_ASSETS`] assets.
pub fn split_assets(input: &[u8]) -> Option<(Vec<&[u8]>, &[u8])> {
    let (count, mut rest) = read_u32(input)?;
    let count = usize::try_from(count).ok()?;
    if count > MAX_ASSETS {
        return None;
    }
    let mut assets = Vec::with_capacity(count);
    for _ in 0..count {
        let (len, tail) = read_u32(rest)?;
        let len = usize::try_from(len).ok()?;
        if len > tail.len() {
            return None;
        }
        let (asset, tail) = tail.split_at(len);
        assets.push(asset);
        rest = tail;
    }
    Some((assets, rest))
}

fn read_u32(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let (head, tail) = bytes.split_first_chunk::<4>()?;
    Some((u32::from_le_bytes(*head), tail))
}

#[doc(hidden)]
pub mod __private {
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    // Instances are discarded after every lease, so buffers handed to the host are leaked on
    // purpose: freeing them would only cost code size.

    pub fn alloc(len: i32) -> i32 {
        let Ok(len) = usize::try_from(len) else {
            return 0;
        };
        let mut buf: Vec<u8> = Vec::new();
        if buf.try_reserve_exact(len.max(1)).is_err() {
            return 0;
        }
        let ptr = buf.as_mut_ptr();
        core::mem::forget(buf);
        ptr as usize as i32
    }

    /// # Safety
    /// `ptr..ptr+len` must be memory previously returned by `alloc` and filled by the host.
    pub unsafe fn run(ptr: i32, len: i32, task: fn(&[u8]) -> Vec<u8>) -> i64 {
        let len = usize::try_from(len).unwrap_or(0);
        let input: &[u8] = if len == 0 {
            &[]
        } else {
            // SAFETY: the host wrote `len` bytes at `ptr`, inside a buffer from `alloc`.
            unsafe { core::slice::from_raw_parts(ptr as u32 as usize as *const u8, len) }
        };
        let out: Box<[u8]> = task(input).into_boxed_slice();
        let out_len = out.len() as u64;
        let out_ptr = Box::leak(out).as_ptr() as usize as u64;
        ((out_ptr << 32) | (out_len & 0xffff_ffff)) as i64
    }
}

/// Exports `nuee_alloc` and `nuee_run` for a task function `fn(&[u8]) -> Vec<u8>`.
///
/// ```
/// extern crate alloc;
/// use alloc::vec::Vec;
///
/// fn run(input: &[u8]) -> Vec<u8> {
///     input.to_vec()
/// }
/// nuee_task::export!(run);
/// # fn main() {}
/// ```
///
/// Tasks are built with `panic = "abort"`: on `wasm32-unknown-unknown` an abort is an
/// `unreachable` instruction, so any panic in the task becomes a trap reported by the host.
#[macro_export]
macro_rules! export {
    ($task:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn nuee_alloc(len: i32) -> i32 {
            $crate::__private::alloc(len)
        }

        /// # Safety
        /// Called by the host with a buffer obtained from `nuee_alloc`.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn nuee_run(ptr: i32, len: i32) -> i64 {
            // SAFETY: forwarded from the host contract above.
            unsafe { $crate::__private::run(ptr, len, $task) }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn splits_hand_built_envelope() {
        let mut env = vec![2, 0, 0, 0, 3, 0, 0, 0, b'a', b'b', b'c', 0, 0, 0, 0];
        env.extend_from_slice(b"input");
        let (assets, input) = split_assets(&env).expect("valid envelope");
        assert_eq!(assets, vec![&b"abc"[..], &b""[..]]);
        assert_eq!(input, b"input");
    }

    #[test]
    fn rejects_malformed_envelopes() {
        assert!(split_assets(&[1, 0]).is_none());
        assert!(split_assets(&[1, 0, 0, 0, 5, 0, 0, 0, 1]).is_none());
        assert!(split_assets(&[17, 0, 0, 0]).is_none());
        assert_eq!(split_assets(&[0, 0, 0, 0, 9]), Some((vec![], &[9u8][..])));
    }
}
