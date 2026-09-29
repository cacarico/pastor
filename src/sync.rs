//! Locks that outlive a panic.
//!
//! A `std::sync` lock is poisoned when a thread panics while holding it, and
//! `unwrap()` on every later `lock()`, `read()` or `write()` then panics too,
//! so one failed request would break every request after it until restart.
//! The data behind pastor's locks stays usable after a panic (a SQLite
//! connection rolls back its open transaction; the rest are plain values
//! replaced whole), so the guard is taken back instead.

use std::sync::LockResult;

/// Takes the guard from a lock result, poisoned or not.
pub trait Recover<G> {
    fn recover(self) -> G;
}

impl<G> Recover<G> for LockResult<G> {
    fn recover(self) -> G {
        self.unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, RwLock};

    #[test]
    fn mutex_and_rwlock_recover_from_a_panic() {
        let m = Arc::new(Mutex::new(1));
        let rw = Arc::new(RwLock::new(1));
        let (m2, rw2) = (Arc::clone(&m), Arc::clone(&rw));
        let r = std::thread::spawn(move || {
            let _m = m2.lock().unwrap();
            let _w = rw2.write().unwrap();
            panic!("poison both");
        })
        .join();
        assert!(r.is_err());
        assert!(m.is_poisoned() && rw.is_poisoned());
        *m.lock().recover() += 1;
        *rw.write().recover() += 1;
        assert_eq!(*m.lock().recover(), 2);
        assert_eq!(*rw.read().recover(), 2);
    }
}
