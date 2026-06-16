//! Process-identity helper used to defend against PID reuse.
//!
//! A recorded PID alone is not a stable identity: after an unclean shutdown the
//! kernel can recycle it for an unrelated process. Pairing the PID with the
//! process start time gives a token that survives reuse — a recycled PID has a
//! different start time, so liveness checks can tell "our process is alive" from
//! "some stranger now holds that number."

/// Return a stable start-time token for `pid`, or `None` if it cannot be read
/// (process gone, permission denied, or unsupported platform). The unit is
/// platform-defined; only equality against a previously recorded value is
/// meaningful.
pub fn process_start_time(pid: u32) -> Option<u64> {
    platform::process_start_time(pid)
}

#[cfg(target_os = "macos")]
mod platform {
    pub fn process_start_time(pid: u32) -> Option<u64> {
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`, which is
        // a zeroed proc_bsdinfo of exactly that size. A failure returns a value
        // other than `size`, which we reject before reading any field.
        unsafe {
            let mut info: libc::proc_bsdinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let written = libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            );
            if written == size {
                Some(
                    (info.pbi_start_tvsec as u64) << 20 | (info.pbi_start_tvusec as u64 & 0xf_ffff),
                )
            } else {
                None
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    pub fn process_start_time(pid: u32) -> Option<u64> {
        // Field 22 of /proc/<pid>/stat is starttime in clock ticks since boot.
        // comm (field 2) may contain spaces/parens, so parse after the last ')'.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = &stat[stat.rfind(')')? + 1..];
        after_comm.split_whitespace().nth(19)?.parse().ok()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    pub fn process_start_time(_pid: u32) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_start_time_is_stable_and_present() {
        let pid = std::process::id();
        let first = process_start_time(pid).expect("own start time should be readable");
        let second = process_start_time(pid).expect("own start time should be readable");
        assert_eq!(first, second, "start time must be stable across calls");
    }

    #[test]
    fn unused_pid_has_no_start_time() {
        // PID 0 is the scheduler/swapper and is never a normal user process;
        // proc_pidinfo / /proc lookups fail for it.
        assert_eq!(process_start_time(0), None);
    }
}
