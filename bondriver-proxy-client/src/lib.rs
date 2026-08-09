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

use std::sync::Once;

use bondriver::interface::IBonDriver;

/// Create and return a pointer to a **new** BonDriver instance.
///
/// This is the main entry point called by the host application.
/// Note: The C++ declaration returns IBonDriver*, which is the base class.
///
/// Every call returns an independent object with its own server connection,
/// ring buffer and channel state.  Returning a shared singleton (as this used
/// to) breaks any host that opens the driver more than once — notably a
/// cascaded recisdb-proxy with `max_instances > 1`, where two readers then
/// consume from the same ring buffer and each receives a torn half of the
/// transport stream.
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
    // Process-wide setup, done once no matter how many instances are created.
    static PROCESS_INIT: Once = Once::new();
    PROCESS_INIT.call_once(|| {
        logging::init_file_logger();
        file_log!(info, "CreateBonDriver called (first call)");

        // Set up panic hook to log panics to file
        std::panic::set_hook(Box::new(|info| {
            logging::log_panic(info);
        }));

        let _ = env_logger::try_init();
    });

    // Each call gets its own instance (own connection + own ring buffer).
    let instance_ptr = bondriver::exports::create_instance();
    if instance_ptr.is_null() {
        file_log!(error, "CreateBonDriver: failed to create instance");
        return instance_ptr;
    }

    // Debug: dump the vtable/RTTI layout once.  It describes the *static*
    // vtable shared by every instance, so repeating it per instance would only
    // spam the log.
    #[cfg(windows)]
    {
        static VTABLE_DUMP: Once = Once::new();
        VTABLE_DUMP.call_once(|| unsafe {
            use bondriver::exports::get_vtable_ptr;
            let vtbl_head = get_vtable_ptr();
            file_log!(info, "sizeof(IBonDriver3Vtbl): {} bytes", std::mem::size_of::<bondriver::interface::IBonDriver3Vtbl>());
            file_log!(info, "sizeof(IBonDriver3VtblWithRTTI): {} bytes", std::mem::size_of::<bondriver::interface::IBonDriver3VtblWithRTTI>());
            file_log!(info, "get_vtable_ptr(): {:p}", vtbl_head);

            // vtable[-1] must point to the RTTI Complete Object Locator.
            let vtbl_ptr_raw = vtbl_head as *const usize;
            file_log!(info, "vtbl[-1] (RTTI locator ptr): 0x{:016x}", *vtbl_ptr_raw.offset(-1));

            let vtbl = &*vtbl_head;
            file_log!(info, "vtbl.base.base.open_tuner: {:?}", vtbl.base.base.open_tuner.map(|f| f as *const ()));
            file_log!(info, "vtbl.base.base.release: {:?}", vtbl.base.base.release.map(|f| f as *const ()));
            file_log!(info, "vtbl.base.get_tuner_name: {:?}", vtbl.base.get_tuner_name.map(|f| f as *const ()));

            let vtbl_bytes = vtbl_head as *const u8;
            file_log!(info, "Raw vtable dump:");
            for i in 0..20 {
                let ptr_addr = vtbl_bytes.add(i * 8) as *const usize;
                file_log!(info, "  vtbl[{}] = 0x{:016x}", i, *ptr_addr);
            }
        });
    }

    file_log!(info, "Returning instance pointer: {:p}", instance_ptr);
    instance_ptr
}

// Note: We don't define DllMain - let the CRT handle DLL initialization.
// Logging is initialized on first call to CreateBonDriver.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_create_bondriver_call_returns_an_independent_instance() {
        let ptr = CreateBonDriver();
        assert!(!ptr.is_null());

        // A second call must NOT hand back the same object: two hosts (or two
        // reader slots of one cascaded proxy) sharing a single connection and
        // ring buffer would tear each other's transport stream apart.
        let ptr2 = CreateBonDriver();
        assert!(!ptr2.is_null());
        assert_ne!(ptr, ptr2);
    }
}
