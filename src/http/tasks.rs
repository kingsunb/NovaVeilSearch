//! Retained HTTP search results. A response connection is only a subscriber;
//! dropping it never cancels the worker or releases its concurrency permit.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::StatusCode;
use serde_json::Value;
use tokio::sync::{watch, Mutex, OwnedSemaphorePermit};
use tokio::time::Instant;

const UNCLAIMED_RESULT_TTL: Duration = Duration::from_secs(30 * 60);
const DELIVERED_RESULT_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_TASKS: usize = 128;
const MAX_RESULT_BYTES: usize = 32 * 1024 * 1024;

pub(super) type SharedTasks = Arc<Mutex<TaskStore>>;

#[derive(Clone)]
pub(super) struct TaskResult {
    pub status: StatusCode,
    pub body: Bytes,
    pub failed: bool,
}

impl TaskResult {
    pub fn new(status: StatusCode, body: Value) -> Self {
        let failed = !status.is_success() || body.get("error").is_some();
        Self {
            status,
            body: Bytes::from(body.to_string()),
            failed,
        }
    }
}

pub(super) struct TaskHandle {
    pub id: String,
    receiver: watch::Receiver<Option<TaskResult>>,
}

impl TaskHandle {
    pub fn result(&self) -> Option<TaskResult> {
        self.receiver.borrow().clone()
    }

    pub async fn wait(&mut self) -> Option<TaskResult> {
        loop {
            if let Some(result) = self.result() {
                return Some(result);
            }
            if self.receiver.changed().await.is_err() {
                return self.result();
            }
        }
    }
}

struct TaskEntry {
    owner: [u8; 32],
    fingerprint: [u8; 32],
    key: Option<String>,
    sender: watch::Sender<Option<TaskResult>>,
    finished_at: Option<Instant>,
    first_delivered_at: Option<Instant>,
}

impl TaskEntry {
    fn expires_at(&self) -> Option<Instant> {
        match self.first_delivered_at {
            Some(at) => Some(at + DELIVERED_RESULT_TTL),
            None => self.finished_at.map(|at| at + UNCLAIMED_RESULT_TTL),
        }
    }

    fn handle(&self, id: &str) -> TaskHandle {
        TaskHandle {
            id: id.to_string(),
            receiver: self.sender.subscribe(),
        }
    }
}

#[derive(Default)]
pub(super) struct TaskStore {
    entries: HashMap<String, TaskEntry>,
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let hash = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut output = [0; 32];
    output.copy_from_slice(hash.as_ref());
    output
}

impl TaskStore {
    /// Start the five-minute replay window after the first complete delivery.
    /// Repeated deliveries cannot extend expiry or revive an expired record.
    pub fn mark_delivered(&mut self, id: &str) {
        self.prune();
        if let Some(entry) = self.entries.get_mut(id) {
            if entry.finished_at.is_some() {
                entry.first_delivered_at.get_or_insert_with(Instant::now);
            }
        }
    }

    pub fn get(&mut self, id: &str, token: &str) -> Option<TaskHandle> {
        self.prune();
        let entry = self.entries.get(id)?;
        (entry.owner == digest(token.as_bytes())).then(|| entry.handle(id))
    }

    fn register(
        &mut self,
        token: &str,
        request: &Value,
        key: Option<String>,
    ) -> Result<(TaskHandle, bool), (StatusCode, &'static str)> {
        self.prune();
        let owner = digest(token.as_bytes());
        let fingerprint = digest(request.to_string().as_bytes());
        if let Some(key) = key.as_ref() {
            for (id, entry) in &self.entries {
                if entry.owner == owner && entry.key.as_ref() == Some(key) {
                    if entry.fingerprint != fingerprint {
                        return Err((
                            StatusCode::CONFLICT,
                            "Idempotency-Key was already used for a different request",
                        ));
                    }
                    return Ok((entry.handle(id), false));
                }
            }
        }
        // Never evict running work: every accepted task must remain pollable.
        if self.entries.len() >= MAX_TASKS && !self.evict_oldest_result() {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "search task store is full"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = watch::channel(None);
        self.entries.insert(
            id.clone(),
            TaskEntry {
                owner,
                fingerprint,
                key,
                sender,
                finished_at: None,
                first_delivered_at: None,
            },
        );
        Ok((TaskHandle { id, receiver }, true))
    }

    fn finish(&mut self, id: &str, result: TaskResult) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.finished_at = Some(Instant::now());
            // send_replace also stores a result when all clients disconnected.
            let _ = entry.sender.send_replace(Some(result));
        }
        self.prune();
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| entry.expires_at().map_or(true, |at| now < at));
        while self.result_bytes() > MAX_RESULT_BYTES && self.evict_oldest_result() {}
    }

    fn result_bytes(&self) -> usize {
        self.entries
            .values()
            .map(|entry| entry.sender.borrow().as_ref().map_or(0, |r| r.body.len()))
            .sum()
    }

    fn evict_oldest_result(&mut self) -> bool {
        let oldest = self
            .entries
            .iter()
            .filter_map(|(id, entry)| entry.finished_at.map(|at| (id.clone(), at)))
            .min_by_key(|(_, at)| *at);
        if let Some((id, _)) = oldest {
            self.entries.remove(&id);
            true
        } else {
            false
        }
    }
}

pub(super) fn new_store() -> SharedTasks {
    let store = Arc::new(Mutex::new(TaskStore::default()));
    let weak = Arc::downgrade(&store);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            let Some(store) = weak.upgrade() else {
                break;
            };
            store.lock().await.prune();
        }
    });
    store
}

/// Registration and spawning have no intervening await, so cancellation of a
/// POST handler cannot leave an accepted task without a worker. The supervisor
/// retains the permit even after the response subscriber disappears.
pub(super) async fn start<F>(
    store: SharedTasks,
    token: &str,
    request: &Value,
    key: Option<String>,
    permit: OwnedSemaphorePermit,
    work: F,
    failure: TaskResult,
) -> Result<TaskHandle, (StatusCode, &'static str)>
where
    F: Future<Output = TaskResult> + Send + 'static,
{
    let (handle, fresh) = store.lock().await.register(token, request, key)?;
    if fresh {
        let id = handle.id.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let result = tokio::spawn(work).await.unwrap_or(failure);
            store.lock().await.finish(&id, result);
        });
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::{oneshot, Semaphore};

    fn reply() -> TaskResult {
        TaskResult::new(StatusCode::OK, json!({ "result": "complete answer" }))
    }

    #[test]
    fn retries_reuse_results_but_cannot_change_request_or_owner() {
        let mut store = TaskStore::default();
        let request = json!({ "endpoint": "mcp", "query": "first" });
        let (first, fresh) = store
            .register("owner", &request, Some("key".to_string()))
            .unwrap();
        assert!(fresh);
        let (running, fresh) = store
            .register("owner", &request, Some("key".to_string()))
            .unwrap();
        assert!(!fresh);
        assert_eq!(first.id, running.id);
        assert!(running.result().is_none());
        assert!(store.get(&first.id, "another-owner").is_none());
        let conflict = store.register("owner", &json!({ "query": "changed" }), Some("key".into()));
        assert_eq!(conflict.err().unwrap().0, StatusCode::CONFLICT);

        let expected = reply();
        store.finish(&first.id, expected.clone());
        let (completed, fresh) = store
            .register("owner", &request, Some("key".to_string()))
            .unwrap();
        assert!(!fresh);
        assert_eq!(completed.result().unwrap().body, expected.body);
        assert_eq!(store.result_bytes(), expected.body.len());
        let (other, fresh) = store
            .register("another-owner", &request, Some("key".to_string()))
            .unwrap();
        assert!(fresh);
        assert_ne!(other.id, first.id);
    }

    #[test]
    fn capacity_evicts_completed_results_and_preserves_running_work() {
        let mut store = TaskStore::default();
        let mut handles = Vec::new();
        for _ in 0..MAX_TASKS {
            handles.push(store.register("owner", &json!({}), None).unwrap().0);
        }
        assert_eq!(
            store.register("owner", &json!({}), None).err().unwrap().0,
            StatusCode::SERVICE_UNAVAILABLE,
        );
        store.finish(&handles[0].id, reply());
        store.register("owner", &json!({}), None).unwrap();
        assert!(store.get(&handles[0].id, "owner").is_none());
        for handle in handles.iter().skip(1) {
            assert!(store.get(&handle.id, "owner").is_some());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn first_delivery_starts_a_fixed_five_minute_replay_window() {
        let mut store = TaskStore::default();
        let request = json!({ "query": "test" });
        let (task, _) = store
            .register("owner", &request, Some("key".into()))
            .unwrap();
        store.mark_delivered(&task.id);
        assert!(store.get(&task.id, "owner").is_some());
        assert!(store.entries[&task.id].first_delivered_at.is_none());

        let expected = reply();
        store.finish(&task.id, expected.clone());
        // Even a late first delivery gets five full minutes for retries.
        tokio::time::advance(Duration::from_secs(29 * 60)).await;
        store.mark_delivered(&task.id);
        let delivered_at = store.entries[&task.id].first_delivered_at;
        assert!(delivered_at.is_some());
        assert_eq!(store.result_bytes(), expected.body.len());
        let (retry, fresh) = store
            .register("owner", &request, Some("key".into()))
            .unwrap();
        assert!(!fresh);
        assert_eq!(retry.id, task.id);
        assert_eq!(retry.result().unwrap().body, expected.body);
        assert!(store.get(&task.id, "another-owner").is_none());

        tokio::time::advance(Duration::from_secs(4 * 60 + 59)).await;
        store.mark_delivered(&task.id);
        assert_eq!(store.entries[&task.id].first_delivered_at, delivered_at);
        let cached = store.get(&task.id, "owner").unwrap().result().unwrap();
        assert_eq!(cached.body, expected.body);
        tokio::time::advance(Duration::from_secs(1)).await;
        store.mark_delivered(&task.id); // Cannot revive a record at its deadline.
        assert!(store.entries.is_empty());
        assert_eq!(store.result_bytes(), 0);
        assert!(store.get(&task.id, "owner").is_none());
        let (next, fresh) = store
            .register("owner", &request, Some("key".into()))
            .unwrap();
        assert!(fresh);
        assert_ne!(task.id, next.id);
    }

    #[test]
    fn result_byte_budget_evicts_oldest_reply_but_keeps_running_tasks() {
        let mut store = TaskStore::default();
        let (running, _) = store.register("owner", &json!({}), None).unwrap();
        let (first, _) = store.register("owner", &json!({}), None).unwrap();
        let (second, _) = store.register("owner", &json!({}), None).unwrap();
        let large_reply = TaskResult {
            status: StatusCode::OK,
            body: Bytes::from(vec![b'x'; MAX_RESULT_BYTES / 2 + 1]),
            failed: false,
        };
        store.finish(&first.id, large_reply.clone());
        store.entries.get_mut(&first.id).unwrap().finished_at =
            Some(Instant::now() - Duration::from_secs(1));
        store.finish(&second.id, large_reply);
        assert!(store.result_bytes() <= MAX_RESULT_BYTES);
        assert!(store.get(&first.id, "owner").is_none());
        assert!(store.get(&second.id, "owner").is_some());
        assert!(store.get(&running.id, "owner").is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_store_cleans_unclaimed_results_after_thirty_minutes() {
        let store = new_store();
        let id = {
            let mut store = store.lock().await;
            let (task, _) = store.register("owner", &json!({}), None).unwrap();
            store.finish(&task.id, reply());
            task.id
        };
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(29 * 60 + 59)).await;
        tokio::task::yield_now().await;
        // Inspect directly: get()/register() would prune and mask a broken sweeper.
        assert!(store.lock().await.entries.contains_key(&id));
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        let store = store.lock().await;
        assert!(store.entries.is_empty());
        assert_eq!(store.result_bytes(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_store_cleans_delivered_results_after_five_minutes() {
        let store = new_store();
        let id = {
            let mut store = store.lock().await;
            let (task, _) = store.register("owner", &json!({}), None).unwrap();
            store.finish(&task.id, reply());
            store.mark_delivered(&task.id);
            task.id
        };
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(4 * 60 + 59)).await;
        tokio::task::yield_now().await;
        assert!(store.lock().await.entries.contains_key(&id));
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        let store = store.lock().await;
        assert!(store.entries.is_empty());
        assert_eq!(store.result_bytes(), 0);
    }

    #[test]
    fn polling_does_not_extend_completion_expiry() {
        let mut store = TaskStore::default();
        let (task, _) = store.register("owner", &json!({}), None).unwrap();
        store.finish(&task.id, reply());
        let finished_at = store.entries[&task.id].finished_at;
        assert!(store.get(&task.id, "owner").is_some());
        assert_eq!(store.entries[&task.id].finished_at, finished_at);
        store.entries.get_mut(&task.id).unwrap().finished_at =
            Some(Instant::now() - UNCLAIMED_RESULT_TTL - Duration::from_secs(1));
        assert!(store.get(&task.id, "owner").is_none());
    }

    #[tokio::test]
    async fn disconnect_keeps_work_and_permit_then_retains_full_reply() {
        let store = new_store();
        let semaphore = Arc::new(Semaphore::new(1));
        let (finish, gate) = oneshot::channel();
        let expected = reply();
        let output = expected.clone();
        let task = start(
            store.clone(),
            "owner",
            &json!({ "query": "keep searching" }),
            None,
            semaphore.clone().try_acquire_owned().unwrap(),
            async move {
                gate.await.unwrap();
                output
            },
            TaskResult::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "failed" }),
            ),
        )
        .await
        .unwrap();
        let id = task.id.clone();
        drop(task); // The original downstream connection disappears.
        assert_eq!(semaphore.available_permits(), 0);
        assert!(store
            .lock()
            .await
            .get(&id, "owner")
            .unwrap()
            .result()
            .is_none());
        finish.send(()).unwrap();
        // A later poll attaches even though no subscriber existed at finish.
        let permit = tokio::time::timeout(Duration::from_secs(1), semaphore.acquire())
            .await
            .unwrap()
            .unwrap();
        let cached = store
            .lock()
            .await
            .get(&id, "owner")
            .unwrap()
            .result()
            .unwrap();
        assert_eq!(cached.body, expected.body);
        assert!(!cached.failed);
        drop(permit);
    }

    #[tokio::test]
    async fn panicked_worker_becomes_a_retained_failure_and_releases_permit() {
        let store = new_store();
        let semaphore = Arc::new(Semaphore::new(1));
        let failure = TaskResult::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "failed" }),
        );
        let mut task = start(
            store.clone(),
            "owner",
            &json!({}),
            None,
            semaphore.clone().try_acquire_owned().unwrap(),
            async { panic!("simulated worker panic") },
            failure.clone(),
        )
        .await
        .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), task.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(result.failed);
        assert_eq!(result.body, failure.body);
        let permit = tokio::time::timeout(Duration::from_secs(1), semaphore.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
    }
}
