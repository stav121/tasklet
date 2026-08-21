//! A small, cheaply-clonable typed store for passing data between tasks and steps.
//!
//! Steps are `'static` closures, so anything they need from the outside has to be
//! captured by value. A [`Blackboard`] is the shared thing you capture: clone it into
//! as many task/step closures as you like and they all read and write the same
//! underlying map. It replaces the ad-hoc `Arc<Mutex<HashMap<...>>>` boilerplate that
//! would otherwise be repeated at every call site.
//!
//! Values are stored by string key and retrieved by type: [`Blackboard::get`] returns
//! `Some` only if a value under that key was stored with the requested type.
//!
//! # Examples
//!
//! ```
//! use tasklet::Blackboard;
//!
//! let board = Blackboard::new();
//! board.set("attempts", 0u32);
//!
//! // A clone shares the same storage.
//! let other = board.clone();
//! other.set("attempts", 3u32);
//!
//! assert_eq!(board.get::<u32>("attempts"), Some(3));
//! // Reading with the wrong type yields `None`.
//! assert_eq!(board.get::<String>("attempts"), None);
//! ```

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A cheaply-clonable, thread-safe, typed key/value store shared between tasks and
/// steps.
///
/// Cloning a `Blackboard` is cheap (it shares the same storage via an [`Arc`]); pass a
/// clone into each closure that needs access.
#[derive(Clone, Default)]
pub struct Blackboard {
    inner: Arc<Mutex<HashMap<String, Box<dyn Any + Send>>>>,
}

impl Blackboard {
    /// Create a new, empty blackboard.
    pub fn new() -> Self {
        Blackboard {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Store `value` under `key`, replacing any previous value regardless of its type.
    pub fn set<T>(&self, key: &str, value: T)
    where
        T: Send + 'static,
    {
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_string(), Box::new(value));
    }

    /// Retrieve a clone of the value stored under `key`, if one exists and was stored
    /// as type `T`. Returns `None` if the key is absent or holds a different type.
    pub fn get<T>(&self, key: &str) -> Option<T>
    where
        T: Clone + Send + 'static,
    {
        self.inner
            .lock()
            .unwrap()
            .get(key)
            .and_then(|value| value.downcast_ref::<T>())
            .cloned()
    }

    /// Return `true` if a value is stored under `key` (of any type).
    pub fn contains(&self, key: &str) -> bool {
        self.inner.lock().unwrap().contains_key(key)
    }

    /// Remove the value stored under `key`. Returns `true` if a value was present.
    pub fn remove(&self, key: &str) -> bool {
        self.inner.lock().unwrap().remove(key).is_some()
    }

    /// Retrieve the value under `key` if present and of type `T`, otherwise store and
    /// return `default`.
    pub fn get_or_insert<T>(&self, key: &str, default: T) -> T
    where
        T: Clone + Send + 'static,
    {
        let mut map = self.inner.lock().unwrap();
        if let Some(existing) = map.get(key).and_then(|v| v.downcast_ref::<T>()) {
            return existing.clone();
        }
        map.insert(key.to_string(), Box::new(default.clone()));
        default
    }

    /// The number of stored keys.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Whether the blackboard holds no values.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    /// A snapshot of all currently stored keys.
    pub fn keys(&self) -> Vec<String> {
        self.inner.lock().unwrap().keys().cloned().collect()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn set_and_get_roundtrips() {
        let board = Blackboard::new();
        board.set("count", 42u32);
        board.set("label", "hello".to_string());
        assert_eq!(board.get::<u32>("count"), Some(42));
        assert_eq!(board.get::<String>("label"), Some("hello".to_string()));
    }

    #[test]
    fn missing_key_is_none() {
        let board = Blackboard::new();
        assert_eq!(board.get::<u32>("nope"), None);
        assert!(!board.contains("nope"));
    }

    #[test]
    fn wrong_type_is_none() {
        let board = Blackboard::new();
        board.set("count", 42u32);
        // Stored as u32; asking for a String must not succeed.
        assert_eq!(board.get::<String>("count"), None);
        // The value is still there under the correct type.
        assert_eq!(board.get::<u32>("count"), Some(42));
    }

    #[test]
    fn set_overwrites() {
        let board = Blackboard::new();
        board.set("count", 1u32);
        board.set("count", 2u32);
        assert_eq!(board.get::<u32>("count"), Some(2));
    }

    #[test]
    fn clones_share_storage() {
        let board = Blackboard::new();
        let clone = board.clone();
        board.set("shared", 7i64);
        assert_eq!(clone.get::<i64>("shared"), Some(7));
        clone.set("shared", 8i64);
        assert_eq!(board.get::<i64>("shared"), Some(8));
    }

    #[test]
    fn remove_reports_presence() {
        let board = Blackboard::new();
        board.set("k", 1u8);
        assert!(board.remove("k"));
        assert!(!board.remove("k"));
        assert!(!board.contains("k"));
    }

    #[test]
    fn get_or_insert_inserts_then_reads() {
        let board = Blackboard::new();
        assert_eq!(board.get_or_insert("k", 10u32), 10);
        // Second call returns the stored value, ignoring the new default.
        assert_eq!(board.get_or_insert("k", 99u32), 10);
    }

    #[test]
    fn len_keys_and_empty() {
        let board = Blackboard::new();
        assert!(board.is_empty());
        board.set("a", 1u32);
        board.set("b", 2u32);
        assert_eq!(board.len(), 2);
        let mut keys = board.keys();
        keys.sort();
        assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);
    }
}
