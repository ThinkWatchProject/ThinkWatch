//! Key-space helpers that work the same on one Redis node and on a
//! Redis Cluster.
//!
//! On a cluster, `SCAN` walks only the node it is sent to, and a `DEL`
//! naming keys of different hash slots is refused (`CROSSSLOT`). A
//! pattern delete therefore has to scan every primary and delete slot
//! by slot.

use std::collections::BTreeMap;
use std::future::Future;

use fred::clients::Client;
use fred::interfaces::{ClientLike, KeysInterface};
use fred::types::Key;
use fred::types::scan::ScanType;
use futures::{Stream, StreamExt};

/// Keys deleted per `DEL` on a single node.
const BATCH: usize = 256;

/// A pattern delete during which a `SCAN` or `DEL` failed. The keys
/// the other commands found were deleted all the same.
#[derive(Debug, thiserror::Error)]
#[error("{failures} SCAN/DEL command(s) failed, {deleted} key(s) deleted; first error: {first}")]
pub struct IncompleteDelete {
    /// Keys deleted despite the failures.
    pub deleted: usize,
    /// Commands that failed.
    pub failures: usize,
    /// The first failure.
    pub first: fred::error::Error,
}

/// Delete every key matching `pattern` (a `SCAN MATCH` glob), of
/// `kind` when given. Returns how many were deleted. Keys written while
/// the scan runs may survive it, as with any `SCAN`.
///
/// A failed `SCAN` (of one node) or `DEL` (of one batch or slot) is
/// logged and the rest of the delete goes on, so one unreachable node
/// or refused batch doesn't leave every later key in place; the error
/// comes at the end, with the count of what was deleted.
pub async fn delete_matching(
    redis: &Client,
    pattern: &str,
    kind: Option<ScanType>,
) -> Result<usize, IncompleteDelete> {
    let clustered = redis.is_clustered();
    let keys = if clustered {
        redis
            .scan_cluster_buffered(pattern.to_owned(), Some(BATCH as u32), kind)
            .boxed()
    } else {
        redis
            .scan_buffered(pattern.to_owned(), Some(BATCH as u32), kind)
            .boxed()
    };
    delete_all(keys, clustered, pattern, |keys| async move {
        redis.del::<u64, _>(keys).await
    })
    .await
}

/// [`delete_matching`] past the choice of client: `del` deletes one
/// batch of keys that a single `DEL` may name.
async fn delete_all<S, D, F>(
    mut keys: S,
    clustered: bool,
    pattern: &str,
    mut del: D,
) -> Result<usize, IncompleteDelete>
where
    S: Stream<Item = Result<Key, fred::error::Error>> + Unpin,
    D: FnMut(Vec<Key>) -> F,
    F: Future<Output = Result<u64, fred::error::Error>>,
{
    let mut tally = Tally::default();
    let mut batch: Vec<Key> = Vec::with_capacity(BATCH);
    while let Some(key) = keys.next().await {
        match key {
            Ok(key) => batch.push(key),
            // On a cluster the other nodes' scans go on; on one node
            // the stream ends after its error.
            Err(e) => tally.failed(pattern, "SCAN", 0, e),
        }
        if batch.len() == BATCH {
            delete(
                &mut tally,
                pattern,
                std::mem::take(&mut batch),
                clustered,
                &mut del,
            )
            .await;
        }
    }
    delete(&mut tally, pattern, batch, clustered, &mut del).await;
    match tally.first {
        None => Ok(tally.deleted),
        Some(first) => Err(IncompleteDelete {
            deleted: tally.deleted,
            failures: tally.failures,
            first,
        }),
    }
}

#[derive(Default)]
struct Tally {
    deleted: usize,
    failures: usize,
    first: Option<fred::error::Error>,
}

impl Tally {
    fn failed(&mut self, pattern: &str, command: &str, keys: usize, e: fred::error::Error) {
        tracing::warn!(
            pattern, command, keys, error = %e,
            "Redis command failed during a pattern delete; deleting the rest"
        );
        self.failures += 1;
        self.first.get_or_insert(e);
    }
}

async fn delete<D, F>(
    tally: &mut Tally,
    pattern: &str,
    keys: Vec<Key>,
    clustered: bool,
    del: &mut D,
) where
    D: FnMut(Vec<Key>) -> F,
    F: Future<Output = Result<u64, fred::error::Error>>,
{
    if keys.is_empty() {
        return;
    }
    let groups = if clustered {
        by_slot(keys).into_values().collect()
    } else {
        vec![keys]
    };
    for keys in groups {
        let n = keys.len();
        match del(keys).await {
            Ok(deleted) => tally.deleted += deleted as usize,
            Err(e) => tally.failed(pattern, "DEL", n, e),
        }
    }
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
    use fred::error::{Error, ErrorKind};

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

    fn keys(names: &[&str]) -> Vec<Result<Key, Error>> {
        names.iter().map(|k| Ok(Key::from(*k))).collect()
    }

    /// A `DEL` that fails for any batch naming `poison`, and records
    /// every batch it was given.
    fn del_failing_on<'a>(
        poison: &'static str,
        seen: &'a std::sync::Mutex<Vec<Vec<String>>>,
    ) -> impl FnMut(Vec<Key>) -> std::future::Ready<Result<u64, Error>> + 'a {
        move |batch: Vec<Key>| {
            let names: Vec<String> = batch
                .iter()
                .map(|k| k.as_str().unwrap().to_owned())
                .collect();
            let fails = names.iter().any(|k| k == poison);
            let n = names.len() as u64;
            seen.lock().unwrap().push(names);
            std::future::ready(if fails {
                Err(Error::new(ErrorKind::IO, "connection reset"))
            } else {
                Ok(n)
            })
        }
    }

    #[tokio::test]
    async fn a_failed_slot_does_not_stop_the_other_slots() {
        let seen = std::sync::Mutex::new(Vec::new());
        let stream = futures::stream::iter(keys(&["{a}1", "{a}2", "{b}1", "{c}1"]));
        let err = delete_all(stream, true, "*", del_failing_on("{b}1", &seen))
            .await
            .unwrap_err();
        assert_eq!(seen.lock().unwrap().len(), 3, "every slot got its DEL");
        assert_eq!(err.deleted, 3);
        assert_eq!(err.failures, 1);
    }

    #[tokio::test]
    async fn a_failed_batch_does_not_stop_the_later_batches() {
        let seen = std::sync::Mutex::new(Vec::new());
        let names: Vec<String> = (0..BATCH * 2 + 1).map(|i| format!("k{i}")).collect();
        let stream = futures::stream::iter(names.iter().map(|k| Ok(Key::from(k.as_str()))));
        let err = delete_all(stream, false, "*", del_failing_on("k0", &seen))
            .await
            .unwrap_err();
        assert_eq!(seen.lock().unwrap().len(), 3);
        assert_eq!(err.deleted, BATCH + 1);
        assert_eq!(err.failures, 1);
    }

    #[tokio::test]
    async fn a_failed_scan_still_deletes_what_the_others_found() {
        let seen = std::sync::Mutex::new(Vec::new());
        let mut items = keys(&["a", "b"]);
        items.insert(1, Err(Error::new(ErrorKind::IO, "node down")));
        let err = delete_all(
            futures::stream::iter(items),
            false,
            "*",
            del_failing_on("-", &seen),
        )
        .await
        .unwrap_err();
        assert_eq!(err.deleted, 2);
        assert_eq!(err.failures, 1);
        assert!(err.to_string().contains("node down"), "{err}");
    }

    #[tokio::test]
    async fn nothing_failed_is_ok() {
        let seen = std::sync::Mutex::new(Vec::new());
        let stream = futures::stream::iter(keys(&["{a}1", "{b}1"]));
        let deleted = delete_all(stream, true, "*", del_failing_on("-", &seen))
            .await
            .unwrap();
        assert_eq!(deleted, 2);
    }
}
