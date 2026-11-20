//! Best-effort scheduling for the dedicated controller and UDP worker, never CPAL callbacks.
// Native handles remain on their registering thread for their entire lifetime.
#![allow(unsafe_code)]

use std::io;

#[cfg(target_os = "windows")]
mod platform {
    use super::io;
    use std::{ffi::c_void, marker::PhantomData, rc::Rc};

    #[link(name = "avrt")]
    unsafe extern "system" {
        fn AvSetMmThreadCharacteristicsW(task: *const u16, index: *mut u32) -> *mut c_void;
        fn AvSetMmThreadPriority(handle: *mut c_void, priority: i32) -> i32;
        fn AvRevertMmThreadCharacteristics(handle: *mut c_void) -> i32;
    }

    pub struct Priority {
        handle: *mut c_void,
        _owning_thread: PhantomData<Rc<()>>,
    }

    pub fn controller() -> io::Result<Priority> {
        let task: [u16; 10] = [80, 114, 111, 32, 65, 117, 100, 105, 111, 0]; // Pro Audio
        let mut index = 0;
        // SAFETY: task is NUL-terminated UTF-16 and index is a writable DWORD.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &raw mut index) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let priority = Priority {
            handle,
            _owning_thread: PhantomData,
        };
        // SAFETY: handle is registered on this thread; 2 is AVRT_PRIORITY_HIGH.
        if unsafe { AvSetMmThreadPriority(handle, 2) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(priority)
    }

    impl Drop for Priority {
        fn drop(&mut self) {
            // SAFETY: !Send/!Sync guard owns a live handle registered on this thread.
            unsafe {
                AvRevertMmThreadCharacteristics(self.handle);
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::io;
    use std::{marker::PhantomData, rc::Rc};

    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(class: u32, relative_priority: i32) -> i32;
    }

    pub struct Priority(PhantomData<Rc<()>>);

    fn set(class: u32) -> io::Result<()> {
        // SAFETY: valid Darwin QoS class and relative priority 0; affects only this thread.
        let result = unsafe { pthread_set_qos_class_self_np(class, 0) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(result))
        }
    }

    pub fn controller() -> io::Result<Priority> {
        set(0x21)?; // QOS_CLASS_USER_INTERACTIVE; no time-constraint scheduling.
        Ok(Priority(PhantomData))
    }

    impl Drop for Priority {
        fn drop(&mut self) {
            let _ = set(0x15); // QOS_CLASS_DEFAULT before the owning thread exits.
        }
    }

    #[cfg(feature = "telemetry-log")]
    pub fn logger() -> io::Result<()> {
        set(0x11) // QOS_CLASS_UTILITY: do not inherit the controller's interactive QoS.
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod platform {
    use super::io;
    pub struct Priority;
    #[allow(clippy::unnecessary_wraps)]
    pub fn controller() -> io::Result<Priority> {
        Ok(Priority)
    }
}

pub use platform::controller;
pub use platform::controller as udp_worker;

#[cfg(all(target_os = "macos", feature = "telemetry-log"))]
pub use platform::logger;
