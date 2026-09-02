//! Coalesce concurrent downloads for the same destination and integrity.
//!
//! This module is intentionally independent from the downloader engines. It
//! only coordinates the operation supplied by the caller and shares a
//! verified successful result with followers.

use super::fetch::{DownloadResult, Integrity};
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, LazyLock, Weak};
use tokio::sync::{Mutex, Notify};

struct InFlight {
    result: Mutex<Option<DownloadResult>>,
    notify: Notify,
}

static FLIGHTS: LazyLock<dashmap::DashMap<String, Weak<InFlight>>> =
    LazyLock::new(dashmap::DashMap::new);

fn key(destination: &Path, integrity: &Integrity) -> String {
    let destination = if cfg!(windows) {
        destination.display().to_string().to_uppercase()
    } else {
        destination.display().to_string()
    };
    format!(
        "{destination}\0size={:?}\0sha1={:?}\0sha512={:?}\0sha256={:?}\0md5={:?}\0content={:?}",
        integrity.size,
        integrity.sha1,
        integrity.sha512,
        integrity.sha256,
        integrity.md5,
        integrity.content,
    )
}

/// Run an operation as a single flight when an integrity contract exists.
/// Followers receive the leader's verified result and never re-hash the file.
/// Failed flights are not cached; followers retry normally after the leader
/// wakes them.
pub(crate) async fn run<F, Fut>(
    destination: &Path,
    integrity: &Integrity,
    operation: F,
) -> crate::Result<DownloadResult>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = crate::Result<DownloadResult>>,
{
    if integrity.is_empty() {
        return operation().await;
    }

    use dashmap::mapref::entry::Entry;
    let flight_key = key(destination, integrity);
    let (flight, leader) = match FLIGHTS.entry(flight_key) {
        Entry::Occupied(mut entry) => match entry.get().upgrade() {
            Some(flight) => (flight, false),
            None => {
                let flight = Arc::new(InFlight {
                    result: Mutex::new(None),
                    notify: Notify::new(),
                });
                entry.insert(Arc::downgrade(&flight));
                (flight, true)
            }
        },
        Entry::Vacant(entry) => {
            let flight = Arc::new(InFlight {
                result: Mutex::new(None),
                notify: Notify::new(),
            });
            entry.insert(Arc::downgrade(&flight));
            (flight, true)
        }
    };

    if leader {
        let result = operation().await;
        if let Ok(downloaded) = &result {
            *flight.result.lock().await = Some(downloaded.clone());
        }
        flight.notify.notify_waiters();
        result
    } else {
        let notified = flight.notify.notified();
        if let Some(result) = flight.result.lock().await.clone() {
            return Ok(result);
        }
        notified.await;
        if let Some(result) = flight.result.lock().await.clone() {
            Ok(result)
        } else {
            operation().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn concurrent_callers_share_successful_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact.jar");
        let integrity = Integrity::sha1("deadbeef").with_size(7);
        let calls = Arc::new(AtomicUsize::new(0));
        let first_calls = Arc::clone(&calls);
        let first_path = path.clone();
        let first = run(&path, &integrity, move || async move {
            first_calls.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(DownloadResult {
                path: first_path,
                url: "https://example.invalid/artifact.jar".into(),
                source: super::super::fetch::DownloadRouteSource::Official,
                size: 7,
                attempts: 1,
                fallback_count: 0,
            })
        });
        let second_path = dir.path().join("artifact.jar");
        let second = run(&second_path, &integrity, || async {
            panic!("follower must not execute operation")
        });
        let (result, shared) = tokio::join!(first, second);
        assert!(result.is_ok());
        assert_eq!(shared.unwrap().size, 7);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn different_integrity_contracts_use_different_keys() {
        let path = Path::new("artifact.jar");
        assert_ne!(
            key(path, &Integrity::sha1("a")),
            key(path, &Integrity::sha1("b"))
        );
    }
}
