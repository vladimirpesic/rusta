//! Locks that outlive a panic.
//!
//! A `std::sync` lock is poisoned when a thread panics while holding it, and
//! every later `lock().unwrap()` panics in turn: one failed task would take
//! unrelated agent work down with it. The state behind Rusta's locks — sets of
//! answers, queues, records, registries — is updated in single steps, so a
//! poisoned lock still guards consistent data. The policy is therefore to
//! recover: take the guard from the poison error and carry on.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// [`Mutex::lock`], recovering the guard of a poisoned lock.
pub trait LockExt<T: ?Sized> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> LockExt<T> for Mutex<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// [`RwLock::read`] and [`RwLock::write`], recovering the guard of a poisoned
/// lock.
pub trait RwLockExt<T: ?Sized> {
    fn read_or_recover(&self) -> RwLockReadGuard<'_, T>;
    fn write_or_recover(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLockExt<T> for RwLock<T> {
    fn read_or_recover(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_or_recover(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_lock_still_gives_its_data() {
        let lock = std::sync::Arc::new(Mutex::new(vec![1]));
        let poisoner = std::sync::Arc::clone(&lock);
        let _ = std::thread::spawn(move || {
            let mut guard = poisoner.lock().unwrap();
            guard.push(2);
            panic!("poison the lock");
        })
        .join();
        assert!(lock.is_poisoned());
        assert_eq!(*lock.lock_or_recover(), vec![1, 2]);

        let rw = std::sync::Arc::new(RwLock::new(1));
        let poisoner = std::sync::Arc::clone(&rw);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.write().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert_eq!(*rw.read_or_recover(), 1);
        *rw.write_or_recover() = 2;
        assert_eq!(*rw.read_or_recover(), 2);
    }
}
