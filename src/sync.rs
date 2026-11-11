//! A poisoned lock only says another thread panicked; the data is still good and the UI must keep drawing.

use std::sync::{Mutex, MutexGuard};

pub trait LockOk<T> {
    /// `lock()` that ignores poisoning.
    fn lock_ok(&self) -> MutexGuard<'_, T>;
}

impl<T> LockOk<T> for Mutex<T> {
    fn lock_ok(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}
