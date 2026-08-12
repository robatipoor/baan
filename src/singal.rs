use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{BaanError, Result};

pub static TERMINATE: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_sig: libc::c_int) {
    TERMINATE.store(true, Ordering::Relaxed);
}

pub fn install_signal_handlers() -> Result<()> {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal_handler as *const () as libc::sighandler_t;
        sa.sa_flags = 0; // no SA_RESTART — allow EINTR to wake blocking reads
        libc::sigemptyset(&mut sa.sa_mask);

        if libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) == -1 {
            return Err(BaanError::Signal);
        }
        if libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut()) == -1 {
            return Err(BaanError::Signal);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_handler_sets_terminate_flag() {
        TERMINATE.store(false, Ordering::SeqCst);
        signal_handler(libc::SIGTERM);
        assert!(TERMINATE.load(Ordering::SeqCst));
    }

    #[test]
    fn terminate_starts_false_and_is_idempotent() {
        TERMINATE.store(false, Ordering::SeqCst);
        assert!(!TERMINATE.load(Ordering::SeqCst));
        signal_handler(0);
        signal_handler(1);
        assert!(TERMINATE.load(Ordering::SeqCst));
    }
}
