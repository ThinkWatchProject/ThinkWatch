//! Key-space helpers that work the same on one Redis node and on a
//! Redis Cluster.
//!
//! On a cluster, `SCAN` walks only the node it is sent to, and a `DEL`
//! naming keys of different hash slots is refused (`CROSSSLOT`). A
//! pattern delete therefore has to scan every primary and delete slot
//! by slot.

use std::collections::BTreeMap;

use fred::clients::Client;
use fred::interfaces::{ClientLike, KeysInterface};
use fred::types::Key;
use fred::types::scan::ScanType;
use futures::StreamExt;

/// Keys deleted per `DEL` on a single node.
const BATCH: usize = 256;

/// Delete every key matching `pattern` (a `SCAN MATCH` glob), of
/// `kind` when given. Returns how many were deleted. Keys written while
/// the scan runs may survive it, as with any `SCAN`.
pub async fn delete_matching(
    redis: &Client,
    pattern: &str,
    kind: Option<ScanType>,
) -> Result<usize, fred::error::Error> {
    let clustered = redis.is_clustered();
    let mut keys = if clustered {
        redis
            .scan_cluster_buffered(pattern.to_owned(), Some(BATCH as u32), kind)
            .boxed()
    } else {
        redis
            .scan_buffered(pattern.to_owned(), Some(BATCH as u32), kind)
            .boxed()
    };
    let mut batch: Vec<Key> = Vec::with_capacity(BATCH);
    let mut deleted = 0;
    while let Some(key) = keys.next().await {
        batch.push(key?);
        if batch.len() == BATCH {
            deleted += delete(redis, std::mem::take(&mut batch), clustered).await?;
        }
    }
    deleted += delete(redis, batch, clustered).await?;
    Ok(deleted)
}

async fn delete(
    redis: &Client,
    keys: Vec<Key>,
    clustered: bool,
) -> Result<usize, fred::error::Error> {
    if keys.is_empty() {
        return Ok(0);
    }
    if !clustered {
        return Ok(redis.del::<u64, _>(keys).await? as usize);
    }
    let mut deleted = 0;
    for (_, keys) in by_slot(keys) {
        deleted += redis.del::<u64, _>(keys).await? as usize;
    }
    Ok(deleted)
}

/// Group keys by Redis Cluster hash slot: one `DEL` per group is one a
/// cluster accepts.
fn by_slot(keys: Vec<Key>) -> BTreeMap<u16, Vec<Key>> {
    let mut out: BTreeMap<u16, Vec<Key>> = BTreeMap::new();
    for key in keys {
        out.entry(fred::util::redis_keyslot(key.as_bytes()))
            .or_default()
            .push(key);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_of_one_slot_are_deleted_together() {
        let keys: Vec<Key> = ["{a}1", "{a}2", "{b}1", "c"]
            .into_iter()
            .map(Key::from)
            .collect();
        let groups = by_slot(keys);
        assert_eq!(groups.len(), 3);
        let a = fred::util::redis_keyslot(b"a");
        assert_eq!(groups[&a].len(), 2);
        for (slot, keys) in &groups {
            for k in keys {
                assert_eq!(fred::util::redis_keyslot(k.as_bytes()), *slot);
            }
        }
    }
}
