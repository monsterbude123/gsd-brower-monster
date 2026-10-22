/// Returns true when `pid` refers to a live Windows process.
pub fn is_process_alive(pid: u32) -> bool {
    extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> isize;
        fn CloseHandle(hObject: isize) -> i32;
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    if pid == 0 {
        return false;
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return false;
        }
        CloseHandle(handle);
        true
    }
}

/// Clear the inherit flag on this process's own standard handles.
///
/// Rust's `Command::spawn` on Windows always passes `bInheritHandles = TRUE`,
/// which copies *every* inheritable handle in the parent into the child, not
/// just the ones meant for it. A daemon spawned by a CLI whose stdout is a pipe
/// therefore inherits that pipe's write end and holds it open for its whole
/// lifetime, so a reader on the other end (`gsd-browser ... | head`) never sees
/// EOF and the pipeline hangs.
///
/// `Stdio::inherit()` still works afterwards: Rust duplicates the handle with
/// the inherit flag set at spawn time instead of relying on the parent's flag.
pub fn clear_std_handle_inherit_flags() {
    extern "system" {
        fn GetStdHandle(nStdHandle: i32) -> isize;
        fn SetHandleInformation(hObject: isize, dwMask: u32, dwFlags: u32) -> i32;
    }

    const STD_INPUT_HANDLE: i32 = -10;
    const STD_OUTPUT_HANDLE: i32 = -11;
    const STD_ERROR_HANDLE: i32 = -12;
    const HANDLE_FLAG_INHERIT: u32 = 0x00000001;

    for std_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let handle = GetStdHandle(std_handle);
            // 0 is NULL and -1 is INVALID_HANDLE_VALUE: nothing to clear.
            if handle > 0 {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}
