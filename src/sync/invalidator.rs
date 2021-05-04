#![allow(unused)]

use crate::common::{
    thread_pool::{PoolName, ThreadPool, ThreadPoolRegistry},
    unsafe_weak_pointer::UnsafeWeakPointer,
    AccessTime,
};

use super::{base_cache::Inner, ValueEntry};

use parking_lot::{Mutex, RwLock};
use quanta::Instant;
use std::{
    collections::HashMap,
    hash::{BuildHasher, Hash},
    marker::PhantomData,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Weak,
    },
};

// TODO: Do a research on concurrent/persistent data structures that could
// replace RwLock<HashMap<_, _>> to store predicates.
//
// Requirements:
// - A write will not block reads or other writes.
// - Provides a way to iterate through the predicates.
//
// Candidates (concurrent data structure):
// - cht crate's HashMap, once iterator is implemented.
//     - https://github.com/Gregory-Meyer/cht/issues/20
//
// Candidates (persistent data structure):
// - im crate's Vector or HashMap.
// - rpds crate's Vector or RedBlackTreeMap.

pub(crate) type PredicateFun<K, V> = Arc<dyn Fn(&K, &V) -> bool + Send + Sync + 'static>;

pub(crate) struct KeyDateLite<K> {
    key: Arc<K>,
    timestamp: Instant,
}

impl<K> Clone for KeyDateLite<K> {
    fn clone(&self) -> Self {
        Self {
            key: Arc::clone(&self.key),
            timestamp: self.timestamp,
        }
    }
}

impl<K> KeyDateLite<K> {
    pub(crate) fn new(key: &Arc<K>, timestamp: Instant) -> Self {
        Self {
            key: Arc::clone(key),
            timestamp,
        }
    }
}

pub(crate) struct Invalidator<K, V, S> {
    predicates: RwLock<HashMap<u64, PredicateImpl<K, V>>>,
    is_empty: AtomicBool,
    last_key: AtomicU64,
    scan_context: ScanContext<K, V>,
    task_result: Arc<Mutex<Option<InternalInvalidationResult<K, V>>>>,
    is_task_running: Arc<AtomicBool>,
    thread_pool: Arc<ThreadPool>,
    cache: Arc<Mutex<UnsafeWeakPointer>>,
    _marker: PhantomData<S>,
}

impl<K, V, S> Drop for Invalidator<K, V, S> {
    fn drop(&mut self) {
        ThreadPoolRegistry::release_pool(&self.thread_pool);
    }
}

//
// Crate public methods.
//
impl<K, V, S> Invalidator<K, V, S> {
    pub(crate) fn new(cache: Weak<Inner<K, V, S>>) -> Self {
        let thread_pool = ThreadPoolRegistry::acquire_pool(PoolName::Invalidator);
        let cache = Arc::new(Mutex::new(UnsafeWeakPointer::from_weak_arc(cache)));

        Self {
            predicates: RwLock::new(HashMap::new()),
            is_empty: AtomicBool::new(true),
            last_key: AtomicU64::new(std::u64::MAX),
            scan_context: ScanContext::default(),
            task_result: Arc::new(Mutex::new(None)),
            // task_result_snd: snd,
            // task_result_rcv: rcv,
            is_task_running: Arc::new(AtomicBool::new(false)),
            thread_pool,
            cache,
            _marker: PhantomData::default(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.is_empty.load(Ordering::Acquire)
    }

    pub(crate) fn register_predicate(
        &self,
        predicate: PredicateFun<K, V>,
        registered_at: Instant,
    ) -> Result<u64, PredicateRegistrationError> {
        const MAX_RETRY: usize = 10_000;
        let mut tries = 0;
        let mut preds = self.predicates.write();

        while tries < MAX_RETRY {
            // NOTE: fetch_add operation wraps around on overflow.
            let id = self.last_key.fetch_add(1, Ordering::SeqCst);
            if preds.contains_key(&id) {
                tries += 1;

                continue; // Retry
            }
            let pred = PredicateImpl::new(predicate, registered_at);
            preds.insert(id, pred);
            self.is_empty.store(false, Ordering::Release);

            return Ok(id);
        }

        Err(PredicateRegistrationError::NoSpaceLeft)
    }

    pub(crate) fn remove_predicate(&self, id: u64) {
        let mut preds = self.predicates.write();
        preds.remove(&id);
        if preds.is_empty() {
            self.is_empty.store(true, Ordering::Release);
        }
    }

    // This method will be called by the get method of Cache.
    #[inline]
    pub(crate) fn apply_predicates(&self, key: &Arc<K>, entry: &Arc<ValueEntry<K, V>>) -> bool {
        if self.is_empty() {
            false
        } else if let Some(ts) = entry.last_modified() {
            Self::do_apply_predicates(self.predicates.read().values(), key, &entry.value, ts)
        } else {
            false
        }
    }

    pub(crate) fn is_task_running(&self) -> bool {
        self.is_task_running.load(Ordering::Acquire)
    }

    pub(crate) fn submit_task(&self, candidates: Vec<KeyDateLite<K>>, is_truncated: bool)
    where
        K: Hash + Eq + Send + Sync + 'static,
        V: Send + Sync + 'static,
        S: BuildHasher + Send + 'static,
    {
        // Ensure there is no pending task and result.
        assert!(!self.is_task_running());
        assert!(self.task_result.lock().is_none());

        {
            let mut ctx_predicates = self.scan_context.predicates.lock();
            if ctx_predicates.is_empty() {
                let predicates = self
                    .predicates
                    .read()
                    .values()
                    .map(|p| PredicateImplLite::new(p))
                    .collect();
                *ctx_predicates = predicates;
            }
        }

        let task = InvalidationTask {
            cache: Arc::clone(&self.cache),
            scan_context: self.scan_context.clone(),
            candidates,
            is_truncated,
            task_result: Arc::clone(&self.task_result),
            is_running: Arc::clone(&self.is_task_running),
            _marker: PhantomData::<S>::default(),
        };

        self.is_task_running.store(true, Ordering::Release);

        self.thread_pool.pool.execute(move || {
            task.execute();
        });
    }

    pub(crate) fn task_result(&self) -> Option<InvalidationResult<K, V>> {
        assert!(!self.is_task_running());
        match self.task_result.lock().take() {
            Some(InternalInvalidationResult {
                invalidated,
                is_truncated,
                newest_timestamp,
            }) => {
                let mut is_done = false;
                let mut predicates = self.scan_context.predicates.lock();

                if is_truncated {
                    if let Some(ts) = newest_timestamp {
                        // TODO: Remove the predicates from the predicate registry too (not just from the context)
                        let ps = predicates
                            .drain(..)
                            .filter(|p| p.registered_at >= ts)
                            .collect();
                        *predicates = ps;
                    }
                    is_done = predicates.is_empty();
                } else {
                    *predicates = Vec::default();
                    is_done = true;
                }

                Some(InvalidationResult {
                    invalidated,
                    is_done,
                })
            }
            None => None,
        }
    }
}

//
// Private methods.
//
impl<K, V, S> Invalidator<K, V, S> {
    #[inline]
    fn do_apply_predicates<'a, I, P>(predicates: I, key: &K, value: &V, ts: Instant) -> bool
    where
        I: Iterator<Item = &'a P>,
        P: Predicate<K, V> + 'a,
    {
        for predicate in predicates {
            if predicate.is_applicable(ts) && predicate.apply(key, value) {
                return true;
            }
        }
        false
    }
}

struct ScanContext<K, V> {
    predicates: Arc<Mutex<Vec<PredicateImplLite<K, V>>>>,
}

impl<K, V> Clone for ScanContext<K, V> {
    fn clone(&self) -> Self {
        Self {
            predicates: Arc::clone(&self.predicates),
        }
    }
}

impl<K, V> Default for ScanContext<K, V> {
    fn default() -> Self {
        Self {
            predicates: Arc::new(Mutex::new(Vec::default())),
        }
    }
}

trait Predicate<K, V> {
    fn is_applicable(&self, last_modified: Instant) -> bool;
    fn apply(&self, key: &K, value: &V) -> bool;
}

struct PredicateImpl<K, V> {
    f: PredicateFun<K, V>,
    registered_at: Instant,
    // The oldest timestamp of the entries checked by this predicate.
    oldest: AtomicU64,
    // The newest timestamp of the entries checked by this predicate.
    newest: AtomicU64,
}

impl<K, V> PredicateImpl<K, V> {
    fn new(f: PredicateFun<K, V>, registered_at: Instant) -> Self {
        Self {
            f,
            registered_at,
            oldest: AtomicU64::new(std::u64::MAX),
            newest: AtomicU64::new(std::u64::MIN),
        }
    }
}

impl<K, V> Predicate<K, V> for PredicateImpl<K, V> {
    fn is_applicable(&self, last_modified: Instant) -> bool {
        last_modified <= self.registered_at
            && (last_modified.as_u64() < self.oldest.load(Ordering::Acquire)
                || last_modified.as_u64() > self.newest.load(Ordering::Acquire))
    }

    fn apply(&self, key: &K, value: &V) -> bool {
        (self.f)(key, value)
    }
}

// PredicateImplLite is optimized for batch invalidation. Unlike PredicateImpl, it has
// no synchronization primitives such as AtomicU64.
struct PredicateImplLite<K, V> {
    f: PredicateFun<K, V>,
    registered_at: Instant,
    // The oldest timestamp of the entries checked by this predicate.
    oldest: Instant,
    // The newest timestamp of the entries checked by this predicate.
    newest: Instant,
}

impl<K, V> PredicateImplLite<K, V> {
    fn new(pred: &PredicateImpl<K, V>) -> Self {
        Self {
            f: Arc::clone(&pred.f),
            registered_at: pred.registered_at,
            oldest: unsafe { std::mem::transmute(pred.oldest.load(Ordering::Acquire)) },
            newest: unsafe { std::mem::transmute(pred.newest.load(Ordering::Acquire)) },
        }
    }
}

impl<K, V> Predicate<K, V> for PredicateImplLite<K, V> {
    fn is_applicable(&self, last_modified: Instant) -> bool {
        last_modified <= self.registered_at
            && (last_modified < self.oldest || last_modified > self.newest)
    }

    fn apply(&self, key: &K, value: &V) -> bool {
        (self.f)(key, value)
    }
}

pub(crate) trait GetOrRemoveEntry<K, V> {
    fn get_value_entry(&self, key: &Arc<K>) -> Option<Arc<ValueEntry<K, V>>>;

    fn remove_key_value_if<F>(&self, key: &Arc<K>, condition: F) -> Option<Arc<ValueEntry<K, V>>>
    where
        F: FnMut(&Arc<K>, &Arc<ValueEntry<K, V>>) -> bool;
}

struct InvalidationTask<K, V, S> {
    cache: Arc<Mutex<UnsafeWeakPointer>>,
    scan_context: ScanContext<K, V>,
    candidates: Vec<KeyDateLite<K>>,
    is_truncated: bool,
    task_result: Arc<Mutex<Option<InternalInvalidationResult<K, V>>>>,
    is_running: Arc<AtomicBool>,
    _marker: PhantomData<S>,
}

impl<K, V, S> InvalidationTask<K, V, S>
where
    K: Hash + Eq,
    S: BuildHasher,
{
    fn execute(&self) {
        let result;
        let lock = self.cache.lock();
        // Restore the Weak pointer to Inner<K, V, S>.
        let weak = unsafe { lock.as_weak_arc::<Inner<K, V, S>>() };
        if let Some(inner_cache) = weak.upgrade() {
            // TODO: Protect this call with catch_unwind().
            result = self.do_execute(&inner_cache);
            // Avoid to drop the Arc<Inner<K, V, S>>.
            UnsafeWeakPointer::forget_arc(inner_cache);
        } else {
            result = InternalInvalidationResult::default();
            // Avoid to drop the Weak<Inner<K, V, S>>.
            UnsafeWeakPointer::forget_weak_arc(weak);
        }

        *self.task_result.lock() = Some(result);
        self.is_running.store(false, Ordering::Release);
    }

    fn do_execute<C>(&self, cache: &Arc<C>) -> InternalInvalidationResult<K, V>
    where
        Arc<C>: GetOrRemoveEntry<K, V>,
    {
        let predicates = self.scan_context.predicates.lock();
        let mut invalidated = Vec::default();
        let mut newest_timestamp = None;

        for candidate in &self.candidates {
            let key = &candidate.key;
            let ts = candidate.timestamp;
            if Self::apply(&predicates, cache, key, ts) {
                if let Some(entry) = Self::invalidate(cache, key, ts) {
                    invalidated.push(entry)
                }
            }
            newest_timestamp = Some(ts);
        }

        InternalInvalidationResult {
            invalidated,
            is_truncated: self.is_truncated,
            newest_timestamp,
        }
    }

    fn apply<C>(
        predicates: &[PredicateImplLite<K, V>],
        cache: &Arc<C>,
        key: &Arc<K>,
        ts: Instant,
    ) -> bool
    where
        Arc<C>: GetOrRemoveEntry<K, V>,
    {
        if let Some(entry) = cache.get_value_entry(key) {
            if let Some(lm) = entry.last_modified() {
                if lm == ts {
                    return Invalidator::<_, _, S>::do_apply_predicates(
                        predicates.iter(),
                        key,
                        &entry.value,
                        lm,
                    );
                }
            }
        }

        false
    }

    fn invalidate<C>(cache: &Arc<C>, key: &Arc<K>, ts: Instant) -> Option<Arc<ValueEntry<K, V>>>
    where
        Arc<C>: GetOrRemoveEntry<K, V>,
    {
        cache.remove_key_value_if(key, |_, v| {
            if let Some(lm) = v.last_modified() {
                lm == ts
            } else {
                false
            }
        })
    }
}

pub(crate) struct InvalidationResult<K, V> {
    pub(crate) invalidated: Vec<Arc<ValueEntry<K, V>>>,
    pub(crate) is_done: bool,
}

struct InternalInvalidationResult<K, V> {
    invalidated: Vec<Arc<ValueEntry<K, V>>>,
    is_truncated: bool,
    newest_timestamp: Option<Instant>,
}

impl<K, V> Default for InternalInvalidationResult<K, V> {
    fn default() -> Self {
        Self {
            invalidated: Vec::default(),
            is_truncated: false,
            newest_timestamp: None,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum PredicateRegistrationError {
    #[error("The write-order queue is disabled. Please enable it using the builder at the cache creation time")]
    WriteOrderQueueDisabled,
    #[error("No space left in the predicate registry")]
    NoSpaceLeft,
}
