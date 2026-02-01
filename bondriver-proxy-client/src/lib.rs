//! BonDriver_NetworkProxy - Network proxy client for BonDriver.
//!
//! This DLL implements the BonDriver interface and connects to a
//! recisdb-proxy server over TCP to access tuners remotely.

#![allow(non_snake_case)]

mod bondriver;
mod client;
mod config;
#[macro_use]
pub mod logging;

use std::sync::atomic::{AtomicPtr, Ordering};
use std::ptr;

use log::info;

use bondriver::interface::IBonDriver;
use bondriver::exports::get_vtable_ptr;

/// Wrapper struct that holds the vtable pointer.
/// This is laid out in memory exactly like IBonDriver3.
#[repr(C)]
struct BonDriverInstance {
    vtbl: *const bondriver::interface::IBonDriver3Vtbl,
}

// Safety: The vtable is a static constant, so it's safe to share across threads.
unsafe impl Sync for BonDriverInstance {}
unsafe impl Send for BonDriverInstance {}

/// Global instance pointer - initialized on first call to CreateBonDriver.
static INSTANCE_PTR: AtomicPtr<BonDriverInstance> = AtomicPtr::new(ptr::null_mut());

/// Create and return a pointer to the BonDriver instance.
///
/// This is the main entry point called by the host application.
/// Note: The C++ declaration returns IBonDriver*, which is the base class.
#[no_mangle]
pub extern "system" fn CreateBonDriver() -> *mut IBonDriver {
    // **IMPROVEMENT**: Wrap the entire function body in catch_unwind to ensure
    // panics don't propagate into C++ code
    let result = std::panic::catch_unwind(|| {
        create_bondriver_impl()
    });

    match result {
        Ok(ptr) => ptr,
        Err(e) => {
            // Log the panic but return a safe null pointer
            logging::init_file_logger();
            file_log!(error, "PANIC in CreateBonDriver: {:?}", e);
            std::ptr::null_mut()
        }
    }
}

/// Internal implementation of CreateBonDriver with panic safety.
fn create_bondriver_impl() -> *mut IBonDriver {
    // Check if we already have an instance
    let mut instance_ptr = INSTANCE_PTR.load(Ordering::Acquire);

    if instance_ptr.is_null() {
        // First call - initialize logging and allocate instance on heap
        logging::init_file_logger();
        file_log!(info, "CreateBonDriver called (first call)");

        // Set up panic hook to log panics to file
        std::panic::set_hook(Box::new(|info| {
            logging::log_panic(info);
        }));

        let _ = env_logger::try_init();
        info!("BonDriver_NetworkProxy initialized");

        // Allocate on heap using Box::leak for 'static lifetime
        // Use get_vtable_ptr() to get pointer to vtable portion of RTTI structure,
        // which allows vtable[-1] to access the RTTI Complete Object Locator.
        let instance = Box::new(BonDriverInstance {
            vtbl: get_vtable_ptr(),
        });
        let new_ptr = Box::into_raw(instance);

        // Try to store our new instance
        match INSTANCE_PTR.compare_exchange(
            ptr::null_mut(),
            new_ptr,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // We won the race
                instance_ptr = new_ptr;
                file_log!(info, "CreateBonDriver: Created new heap-allocated instance at {:p}", instance_ptr);
            }
            Err(existing) => {
                // Another thread won - drop our instance and use theirs
                unsafe { drop(Box::from_raw(new_ptr)); }
                instance_ptr = existing;
                file_log!(info, "CreateBonDriver: Using existing instance from another thread at {:p}", instance_ptr);
            }
        }
    } else {
        file_log!(info, "CreateBonDriver: Returning existing instance at {:p}", instance_ptr);
    }

    // Debug: log vtable information and sizes
    unsafe {
        let instance = &*instance_ptr;
        file_log!(info, "sizeof(BonDriverInstance): {} bytes", std::mem::size_of::<BonDriverInstance>());
        file_log!(info, "sizeof(IBonDriver3Vtbl): {} bytes", std::mem::size_of::<bondriver::interface::IBonDriver3Vtbl>());
        file_log!(info, "sizeof(IBonDriver3VtblWithRTTI): {} bytes", std::mem::size_of::<bondriver::interface::IBonDriver3VtblWithRTTI>());
        file_log!(info, "INSTANCE address: {:p}", instance_ptr);
        file_log!(info, "INSTANCE.vtbl: {:p}", instance.vtbl);
        file_log!(info, "get_vtable_ptr(): {:p}", get_vtable_ptr());

        // Check vtable[-1] - this should point to the RTTI Complete Object Locator
        let vtbl_ptr_raw = instance.vtbl as *const usize;
        let rtti_ptr = *vtbl_ptr_raw.offset(-1);
        file_log!(info, "vtbl[-1] (RTTI locator ptr): 0x{:016x}", rtti_ptr);

        let vtbl = &*instance.vtbl;
        file_log!(info, "vtbl.base.base.open_tuner: {:?}", vtbl.base.base.open_tuner.map(|f| f as *const ()));
        file_log!(info, "vtbl.base.base.close_tuner: {:?}", vtbl.base.base.close_tuner.map(|f| f as *const ()));
        file_log!(info, "vtbl.base.base.release: {:?}", vtbl.base.base.release.map(|f| f as *const ()));
        file_log!(info, "vtbl.base.get_tuner_name: {:?}", vtbl.base.get_tuner_name.map(|f| f as *const ()));

        // Dump raw vtable memory to verify layout
        let vtbl_ptr = instance.vtbl as *const u8;
        let vtbl_size = std::mem::size_of::<bondriver::interface::IBonDriver3Vtbl>();
        file_log!(info, "IBonDriver3Vtbl size: {} bytes ({} pointers)", vtbl_size, vtbl_size / 8);

        // Dump first 20 function pointers (160 bytes on 64-bit)
        file_log!(info, "Raw vtable dump:");
        for i in 0..20 {
            let ptr_addr = vtbl_ptr.add(i * 8) as *const usize;
            file_log!(info, "  vtbl[{}] = 0x{:016x}", i, *ptr_addr);
        }

        // Also dump what's at INSTANCE address
        let instance_dump_ptr = instance_ptr as *const u8;
        file_log!(info, "Raw INSTANCE dump (first 16 bytes):");
        for i in 0..2 {
            let ptr_addr = instance_dump_ptr.add(i * 8) as *const usize;
            file_log!(info, "  INSTANCE[{}] = 0x{:016x}", i, *ptr_addr);
        }
    }

    file_log!(info, "Returning instance pointer: {:p}", instance_ptr);

    // Self-test: try calling GetTunerName through the vtable
    unsafe {
        let instance = &*instance_ptr;
        let vtbl = &*instance.vtbl;
        if let Some(get_tuner_name) = vtbl.base.get_tuner_name {
            file_log!(info, "Self-test: Calling GetTunerName through vtable...");
            let result = get_tuner_name(instance_ptr as *mut std::ffi::c_void);
            file_log!(info, "Self-test: GetTunerName returned {:p}", result);
        } else {
            file_log!(error, "Self-test: GetTunerName is NULL!");
        }
    }

    file_log!(info, "Self-test complete, returning instance");

    // Return the instance as IBonDriver pointer (base class)
    instance_ptr as *mut IBonDriver
}

// Note: We don't define DllMain - let the CRT handle DLL initialization.
// Logging is initialized on first call to CreateBonDriver.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_bondriver() {
        let ptr = CreateBonDriver();
        assert!(!ptr.is_null());

        // Second call should return the same instance
        let ptr2 = CreateBonDriver();
        assert_eq!(ptr, ptr2);
    }
}
