//! COM server plumbing shared by every object in the DLL: a fail-closed panic
//! guard for the FFI boundary, an honest DLL reference counter that backs
//! `DllCanUnloadNow`, and the two required DLL exports.

use core::sync::atomic::{AtomicUsize, Ordering};
use std::panic::AssertUnwindSafe;

use windows::core::{Error, Interface, GUID, HRESULT, PWSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, E_INVALIDARG, E_OUTOFMEMORY, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{CoTaskMemAlloc, IClassFactory};

use crate::factory::ClassFactory;
use crate::guid::CLSID_ARCEN;

/// Live COM objects plus outstanding server locks. `DllCanUnloadNow` returns
/// `S_OK` only when this reaches zero, so LogonUI never unloads us mid-call.
static DLL_OBJECTS: AtomicUsize = AtomicUsize::new(0);
static SERVER_LOCKS: AtomicUsize = AtomicUsize::new(0);

/// Record the birth of a COM object owned by this server.
pub(crate) fn dll_add_ref() {
    DLL_OBJECTS.fetch_add(1, Ordering::AcqRel);
}

/// Record the death of a COM object owned by this server.
pub(crate) fn dll_release() {
    let _ = DLL_OBJECTS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
        count.checked_sub(1)
    });
}

/// Apply an `IClassFactory::LockServer` request.
pub(crate) fn server_lock(lock: bool) {
    if lock {
        SERVER_LOCKS.fetch_add(1, Ordering::AcqRel);
    } else {
        let _ = SERVER_LOCKS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            count.checked_sub(1)
        });
    }
}

fn dll_can_unload() -> bool {
    DLL_OBJECTS.load(Ordering::Acquire) == 0 && SERVER_LOCKS.load(Ordering::Acquire) == 0
}

/// Allocate a NUL-terminated UTF-16 string with the COM task allocator.
///
/// Ownership transfers to the COM caller on success.
pub(crate) fn alloc_wide(value: &str) -> windows::core::Result<PWSTR> {
    if value.encode_utf16().any(|unit| unit == 0) {
        return Err(Error::from_hresult(E_INVALIDARG));
    }
    let units: Vec<u16> = value.encode_utf16().chain(core::iter::once(0)).collect();
    let bytes = units
        .len()
        .checked_mul(core::mem::size_of::<u16>())
        .ok_or_else(|| Error::from_hresult(E_OUTOFMEMORY))?;
    // SAFETY: a non-zero allocation of `bytes` is initialized immediately below.
    let allocation = unsafe { CoTaskMemAlloc(bytes) }.cast::<u16>();
    if allocation.is_null() {
        return Err(Error::from_hresult(E_OUTOFMEMORY));
    }
    // SAFETY: `allocation` names `bytes` writable bytes and `units` contains
    // exactly `bytes / size_of::<u16>()` initialized, non-overlapping units.
    unsafe { core::ptr::copy_nonoverlapping(units.as_ptr(), allocation, units.len()) };
    Ok(PWSTR(allocation))
}

/// Allocate one initialized value with the COM task allocator.
///
/// Ownership transfers to the COM caller on success.
pub(crate) fn alloc_value<T: Copy>(value: T) -> windows::core::Result<*mut T> {
    let bytes = core::mem::size_of::<T>();
    if bytes == 0 {
        return Err(Error::from_hresult(E_INVALIDARG));
    }
    // SAFETY: the allocation is checked and initialized with `value` below.
    let allocation = unsafe { CoTaskMemAlloc(bytes) }.cast::<T>();
    if allocation.is_null() {
        return Err(Error::from_hresult(E_OUTOFMEMORY));
    }
    // SAFETY: `allocation` is aligned and large enough for one `T`.
    unsafe { allocation.write(value) };
    Ok(allocation)
}

/// Run a COM method body, converting any panic into a fail-closed
/// `E_UNEXPECTED` instead of unwinding across the `extern "system"` boundary
/// (which would be undefined behavior).
pub(crate) fn guard<T>(
    context: &str,
    body: impl FnOnce() -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    match std::panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(_) => {
            crate::log::debug(&format!(
                "panic caught in {context}; returning E_UNEXPECTED"
            ));
            Err(Error::from_hresult(E_UNEXPECTED))
        }
    }
}

/// Same as [`guard`] but for the raw `extern "system"` exports that hand back an
/// `HRESULT` directly.
fn guard_hresult(context: &str, body: impl FnOnce() -> HRESULT) -> HRESULT {
    match std::panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(hr) => hr,
        Err(_) => {
            crate::log::debug(&format!(
                "panic caught in {context}; returning E_UNEXPECTED"
            ));
            E_UNEXPECTED
        }
    }
}

/// COM in-proc entry point: hand LogonUI our class factory for our CLSID.
///
/// # Safety
/// Standard COM `DllGetClassObject` contract: `rclsid`/`riid` point to valid
/// GUIDs and `ppv` to a writable pointer slot.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut core::ffi::c_void,
) -> HRESULT {
    guard_hresult("DllGetClassObject", || {
        if ppv.is_null() {
            return E_POINTER;
        }
        // Always clear the out-pointer first so a failure never leaves garbage.
        // SAFETY: `ppv` was checked non-null and COM supplies a writable slot.
        unsafe { *ppv = core::ptr::null_mut() };
        if rclsid.is_null() || riid.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: COM's DllGetClassObject contract supplies readable GUIDs.
        if unsafe { *rclsid } != CLSID_ARCEN {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = ClassFactory::new().into();
        // SAFETY: `riid` and `ppv` satisfy the COM contract checked above.
        unsafe { factory.query(riid, ppv) }
    })
}

/// COM in-proc entry point: report whether the DLL is safe to unload.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    guard_hresult("DllCanUnloadNow", || {
        if dll_can_unload() {
            S_OK
        } else {
            S_FALSE
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn reference_counter_gates_unload() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        let objects_before = DLL_OBJECTS.load(Ordering::Acquire);
        let locks_before = SERVER_LOCKS.load(Ordering::Acquire);
        dll_add_ref();
        assert!(!dll_can_unload());
        server_lock(true);
        dll_release();
        server_lock(false);
        assert_eq!(DLL_OBJECTS.load(Ordering::Acquire), objects_before);
        assert_eq!(SERVER_LOCKS.load(Ordering::Acquire), locks_before);
    }

    #[test]
    fn unmatched_release_and_unlock_do_not_underflow() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        let objects_before = DLL_OBJECTS.load(Ordering::Acquire);
        let locks_before = SERVER_LOCKS.load(Ordering::Acquire);
        if objects_before == 0 {
            dll_release();
            assert_eq!(DLL_OBJECTS.load(Ordering::Acquire), 0);
        }
        if locks_before == 0 {
            server_lock(false);
            assert_eq!(SERVER_LOCKS.load(Ordering::Acquire), 0);
        }
    }
}
