use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
/// The policy of a cache.
pub struct Policy {
    max_capacity: Option<u64>,
    num_segments: usize,
    time_to_live: Option<Duration>,
    time_to_idle: Option<Duration>,
}

impl Policy {
    pub(crate) fn new(
        max_capacity: Option<u64>,
        num_segments: usize,
        time_to_live: Option<Duration>,
        time_to_idle: Option<Duration>,
    ) -> Self {
        Self {
            max_capacity,
            num_segments,
            time_to_live,
            time_to_idle,
        }
    }

    /// Returns the `max_capacity` of the cache.
    pub fn max_capacity(&self) -> Option<u64> {
        self.max_capacity
    }

    #[cfg(feature = "sync")]
    pub(crate) fn set_max_capacity(&mut self, capacity: Option<u64>) {
        self.max_capacity = capacity;
    }

    /// Returns the number of internal segments of the cache.
    pub fn num_segments(&self) -> usize {
        self.num_segments
    }

    #[cfg(feature = "sync")]
    pub(crate) fn set_num_segments(&mut self, num: usize) {
        self.num_segments = num;
    }

    /// Returns the `time_to_live` of the cache.
    pub fn time_to_live(&self) -> Option<Duration> {
        self.time_to_live
    }

    /// Returns the `time_to_idle` of the cache.
    pub fn time_to_idle(&self) -> Option<Duration> {
        self.time_to_idle
    }
}

/// Calculates when cache entries expire. A single expiration time is retained on
/// each entry so that the lifetime of an entry may be extended or reduced by
/// subsequent evaluations.
pub trait Expiry<K, V> {
    /// Specifies that the entry should be automatically removed from the cache once
    /// the duration has elapsed after the entry's creation. Returning `None`
    /// indicates no expiration for the entry.
    ///
    /// **NOTE:** If the cache is configured with `time_to_live` and/or
    /// `time_to_idle` policies, the entry will be evicted after the earliest of the
    /// expiration time calculated by this expiry, the `time_to_live` and
    /// `time_to_idle` policies.
    #[allow(unused_variables)]
    fn expire_after_create(&self, key: &K, value: &V, current_time: Instant) -> Option<Duration> {
        None
    }

    /// Specifies that the entry should be automatically removed from the cache once
    /// the duration has elapsed after its last read. Returning `None` indicates no
    /// expiration for the entry. Returning `current_duration` will not modify the
    /// expiration time.
    ///
    /// **NOTE:** If the cache is configured with `time_to_live` and/or
    /// `time_to_idle` policies, the entry will be evicted after the earliest of the
    /// expiration time calculated by this expiry, the `time_to_live` and
    /// `time_to_idle` policies. Also the `current_duration` takes in account the
    /// `time_to_live` and `time_to_idle` policies.
    #[allow(unused_variables)]
    fn expire_after_read(
        &self,
        key: &K,
        value: &V,
        current_time: Instant,
        current_duration: Option<Duration>,
    ) -> Option<Duration> {
        current_duration
    }

    /// Specifies that the entry should be automatically removed from the cache once
    /// the duration has elapsed after the replacement of its value. Returning `None`
    /// indicates no expiration for the entry. Returning `current_duration` will not
    /// modify the expiration time.
    ///
    /// **NOTE:** If the cache is configured with `time_to_live` and/or
    /// `time_to_idle` policies, the entry will be evicted after the earliest of the
    /// expiration time calculated by this expiry, the `time_to_live` and
    /// `time_to_idle` policies. Also the `current_duration` takes in account the
    /// `time_to_live` and `time_to_idle` policies.
    #[allow(unused_variables)]
    fn expire_after_update(
        &self,
        key: &K,
        value: &V,
        current_time: Instant,
        current_duration: Option<Duration>,
    ) -> Option<Duration> {
        current_duration
    }
}

pub(crate) struct ExpirationPolicy<K, V> {
    time_to_live: Option<Duration>,
    time_to_idle: Option<Duration>,
    expiry: Option<Arc<dyn Expiry<K, V> + Send + Sync + 'static>>,
}

impl<K, V> Default for ExpirationPolicy<K, V> {
    fn default() -> Self {
        Self {
            time_to_live: None,
            time_to_idle: None,
            expiry: None,
        }
    }
}

impl<K, V> Clone for ExpirationPolicy<K, V> {
    fn clone(&self) -> Self {
        Self {
            time_to_live: self.time_to_live,
            time_to_idle: self.time_to_idle,
            expiry: self.expiry.as_ref().map(Arc::clone),
        }
    }
}

impl<K, V> ExpirationPolicy<K, V> {
    #[cfg(test)]
    pub(crate) fn new(
        time_to_live: Option<Duration>,
        time_to_idle: Option<Duration>,
        expiry: Option<Arc<dyn Expiry<K, V> + Send + Sync + 'static>>,
    ) -> Self {
        Self {
            time_to_live,
            time_to_idle,
            expiry,
        }
    }

    /// Returns the `time_to_live` of the cache.
    pub(crate) fn time_to_live(&self) -> Option<Duration> {
        self.time_to_live
    }

    pub(crate) fn set_time_to_live(&mut self, duration: Duration) {
        self.time_to_live = Some(duration);
    }

    /// Returns the `time_to_idle` of the cache.
    pub(crate) fn time_to_idle(&self) -> Option<Duration> {
        self.time_to_idle
    }

    pub(crate) fn set_time_to_idle(&mut self, duration: Duration) {
        self.time_to_idle = Some(duration);
    }

    pub(crate) fn expiry(&self) -> Option<Arc<dyn Expiry<K, V> + Send + Sync + 'static>> {
        self.expiry.as_ref().map(Arc::clone)
    }

    pub(crate) fn set_expiry(&mut self, expiry: Arc<dyn Expiry<K, V> + Send + Sync + 'static>) {
        self.expiry = Some(expiry);
    }
}
