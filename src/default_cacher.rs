use crate::{BloomFilter, Cache, DiskCache};
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;
use tracing::trace;

pub struct DefaultCacher<K: Eq, V, C, D: DiskCache<K, V>> {
    pub bloom_filter: BloomFilter<K>,
    pub hot_cache: C,
    pub disk_cache: D,

    requested: u128,
    in_hotcache: u128,

    _v: PhantomData<V>,
}

impl<K, V, C, D> DefaultCacher<K, V, C, D>
where
    K: Eq + Hash + Clone + Debug + TryFrom<String>,
    V: Clone,
    C: Cache<CacheEntry<K, V>>,
    D: DiskCache<K, V>,
{
    pub fn new(disk_cache: D, items_count: usize, fp_p: f64) -> Self {
        let hot_cache = C::default();
        let bloom_filter: BloomFilter<K> = BloomFilter::new_for_fp_rate(items_count, fp_p);
        Self {
            bloom_filter,
            hot_cache,
            disk_cache,
            requested: 0,
            in_hotcache: 0,

            _v: PhantomData,
        }
    }

    /// create cacher & init bloom filter by scan disk
    pub async fn new_and_init_bloom(
        disk_cache: D,
        items_count: usize,
        fp_p: f64,
    ) -> anyhow::Result<Self> {
        let mut cacher = Self::new(disk_cache, items_count, fp_p);
        cacher.init_bloom_filter().await?;
        Ok(cacher)
    }

    pub async fn store(&mut self, key: K, value: V, weight: usize) -> anyhow::Result<()> {
        // set bloom filter
        self.bloom_filter.set(&key);
        // store into disk
        self.disk_cache.store(&key, value.clone()).await?;
        // insert into hot cache
        self.hot_cache
            .insert_with_weight(CacheEntry::new(key, value), weight);

        Ok(())
    }

    pub async fn load<F>(&mut self, key: &K, weight: usize, mut hot_cache_op: F) -> Option<V>
    where
        F: FnMut(&mut DefaultCacher<K, V, C, D>, &K) -> Option<V>,
    {
        let mut instant = std::time::Instant::now();
        self.requested += 1;
        if !self.bloom_filter.check(key) {
            trace!(key = ?key, elapsed = ?instant.elapsed(), "Not exist in bloom filter");
            return None;
        }

        let check_bloom_elapsed = instant.elapsed();
        trace!(?key, elapsed = ?check_bloom_elapsed, "Check by bloom filter");
        instant += check_bloom_elapsed;

        let maybe = hot_cache_op(self, key);
        let check_hot_cache_elapsed = instant.elapsed();
        trace!(?key, elapsed = ?check_hot_cache_elapsed, "Check hot cache");
        instant += check_hot_cache_elapsed;
        if maybe.is_some() {
            self.in_hotcache += 1;
            let hit_ratio = self.in_hotcache as f64 / self.requested as f64;
            trace!(
                ?key,
                requested = %self.requested,
                in_hotcache = %self.in_hotcache,
                "Got from hot cache, hit_ratio: {:.2}", hit_ratio
            );
            return maybe;
        }

        self.load_from_disk(key, weight, instant).await
    }

    pub fn load_from_hot_cache(&mut self, key: &K) -> Option<V> {
        self.hot_cache
            .find(|item| &item.key == key)
            .map(|item| item.value.clone())
    }

    pub fn lookup_hot_cache(&mut self, key: &K) -> Option<V> {
        self.hot_cache
            .lookup(|item| &item.key == key)
            .map(|item| item.value.clone())
    }

    pub async fn load_from_disk(
        &mut self,
        key: &K,
        weight: usize,
        instant: std::time::Instant,
    ) -> Option<V> {
        self.disk_cache.load(key).await.ok()?.map(|v| {
            trace!(key = ?key, elapsed = ?instant.elapsed(), "Load from disk");
            self.hot_cache
                .insert_with_weight(CacheEntry::new(key.clone(), v.clone()), weight);
            v
        })
    }

    pub fn disk_cacher(&self) -> &D {
        &self.disk_cache
    }

    // TODO: use mmap to sync data
    pub async fn init_bloom_filter(&mut self) -> anyhow::Result<()> {
        let instant = std::time::Instant::now();

        let disk_dir = self.disk_cache.directory();
        let mut dirs = vec![];
        let mut disk_dir = tokio::fs::read_dir(disk_dir).await.unwrap();
        while let Ok(Some(dir_entry)) = disk_dir.next_entry().await {
            if let Ok(file_type) = dir_entry.file_type().await {
                if file_type.is_dir() {
                    dirs.push(dir_entry.path())
                }
            }
        }

        let keys: Vec<_> = tokio::task::spawn_blocking(move || {
            dirs.iter()
                .filter_map(|dir| std::fs::read_dir(dir).ok())
                .flatten()
                .map(Result::unwrap)
                .filter(|dir| dir.file_type().unwrap().is_file())
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect()
        })
        .await?;

        keys.into_iter()
            .filter_map(|k| k.try_into().ok())
            .for_each(|key| self.bloom_filter.set(&key));

        trace!(
            elapsed = ?instant.elapsed(),
            "Inited bloom filter",
        );

        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CacheEntry<K, V> {
    key: K,
    value: V,
}

impl<K: Clone, V: Clone> CacheEntry<K, V> {
    pub fn new(key: K, value: V) -> Self {
        Self { key, value }
    }
}
