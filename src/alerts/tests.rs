use super::*;
use crate::model::StrikePlan;
use crate::store::SqliteStore;
use chrono::{DateTime, Duration, Utc};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use uuid::Uuid;

#[derive(Default)]
struct Faults {
    plan: bool,
    commit: bool,
    forget: bool,
}

struct Repository {
    store: Arc<SqliteStore>,
    faults: Faults,
    prunes: AtomicUsize,
    fail_next_prune: AtomicBool,
}

#[async_trait::async_trait]
impl AlertMessageRepository for Repository {
    async fn record_message(&self, key: &AlertMessageKey, lines: &[AlertLine]) -> Result<()> {
        self.store.record_message(key, lines).await
    }
    async fn plan_strikes(
        &self,
        chat_provider: &str,
        surface: AlertSurface,
        slots: &[BookableSlotId],
    ) -> Result<Vec<StrikePlan>> {
        anyhow::ensure!(!self.faults.plan, "planning failed");
        self.store.plan_strikes(chat_provider, surface, slots).await
    }
    async fn commit_strikes(&self, key: &AlertMessageKey, lines: &[u32]) -> Result<()> {
        anyhow::ensure!(!self.faults.commit, "commit failed");
        self.store.commit_strikes(key, lines).await
    }
    async fn forget_message(&self, key: &AlertMessageKey) -> Result<()> {
        anyhow::ensure!(!self.faults.forget, "forget failed");
        self.store.forget_message(key).await
    }
    async fn prune_ended(&self, now: DateTime<Utc>) -> Result<usize> {
        self.prunes.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(
            !self.fail_next_prune.swap(false, Ordering::SeqCst),
            "prune failed"
        );
        self.store.prune_ended(now).await
    }
}

async fn lifecycle(faults: Faults) -> (Arc<AlertMessageLifecycle>, Arc<Repository>) {
    let repository = Arc::new(Repository {
        store: Arc::new(SqliteStore::open_in_memory().await.unwrap()),
        faults,
        prunes: AtomicUsize::new(0),
        fail_next_prune: AtomicBool::new(false),
    });
    (
        Arc::new(AlertMessageLifecycle::new(repository.clone())),
        repository,
    )
}

fn line() -> AlertLine {
    let starts_at = Utc::now() + Duration::days(1);
    AlertLine {
        club: Some("A Club".into()),
        court_id: Uuid::new_v4(),
        court_name: "Court 1".into(),
        starts_at,
        ends_at: starts_at + Duration::hours(1),
        struck: false,
    }
}

fn slot(line: &AlertLine) -> BookableSlotId {
    BookableSlotId {
        court_id: line.court_id,
        starts_at: line.starts_at,
    }
}

#[tokio::test]
async fn chat_providers_share_pruning_but_edit_only_their_own_messages() {
    let (lifecycle, repository) = lifecycle(Faults::default()).await;
    let first = lifecycle.tracker("first", AlertSurface::DirectMessage);
    let second = lifecycle.tracker("second", AlertSurface::DirectMessage);
    let announced = line();
    first
        .record(Some("77"), "42", std::slice::from_ref(&announced))
        .await;
    second
        .record(Some("77"), "42", std::slice::from_ref(&announced))
        .await;
    let slots = [slot(&announced)];
    let edited = Mutex::new(Vec::new());
    let edit = |message: AlertMessage| {
        assert!(message.lines[0].struck);
        edited.lock().unwrap().push(message.key);
        async { Ok(EditOutcome::Edited) }
    };
    let (a, b) = tokio::join!(
        first.mark_taken(&slots, &edit),
        second.mark_taken(&slots, &edit)
    );
    a.unwrap();
    b.unwrap();
    let mut chat_providers: Vec<_> = edited
        .lock()
        .unwrap()
        .iter()
        .map(|k| k.chat_provider.clone())
        .collect();
    chat_providers.sort();
    assert_eq!(chat_providers, ["first", "second"]);
    assert_eq!(repository.prunes.load(Ordering::SeqCst), 1);
    for chat_provider in ["first", "second"] {
        assert!(
            repository
                .store
                .plan_strikes(chat_provider, AlertSurface::DirectMessage, &slots)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn a_failed_edit_keeps_its_plan_and_does_not_block_other_messages() {
    let (lifecycle, repository) = lifecycle(Faults::default()).await;
    let tracker = lifecycle.tracker("test", AlertSurface::Channel);
    let announced = line();
    for id in ["failed", "sent"] {
        tracker
            .record(Some("room"), id, std::slice::from_ref(&announced))
            .await;
    }
    let slots = [slot(&announced)];
    tracker
        .mark_taken(&slots, |message| async move {
            if message.key.id == "failed" {
                anyhow::bail!("transport failed");
            }
            Ok(EditOutcome::Edited)
        })
        .await
        .unwrap();
    let pending = repository
        .store
        .plan_strikes("test", AlertSurface::Channel, &slots)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].message.key.id, "failed");

    tracker
        .mark_taken(&slots, |_| async { Ok(EditOutcome::Gone) })
        .await
        .unwrap();
    assert!(
        repository
            .store
            .plan_strikes("test", AlertSurface::Channel, &slots)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn failed_edit_persistence_leaves_the_message_tracked() {
    for gone in [false, true] {
        let (lifecycle, repository) = lifecycle(Faults {
            commit: !gone,
            forget: gone,
            ..Faults::default()
        })
        .await;
        let tracker = lifecycle.tracker("test", AlertSurface::DirectMessage);
        let announced = line();
        tracker
            .record(Some("room"), "42", std::slice::from_ref(&announced))
            .await;
        let slots = [slot(&announced)];
        tracker
            .mark_taken(&slots, |_| async {
                Ok(if gone {
                    EditOutcome::Gone
                } else {
                    EditOutcome::Edited
                })
            })
            .await
            .unwrap();
        assert_eq!(
            repository
                .store
                .plan_strikes("test", AlertSurface::DirectMessage, &slots)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn planning_failure_still_prunes_and_failed_pruning_can_be_retried() {
    let (lifecycle, repository) = lifecycle(Faults {
        plan: true,
        ..Faults::default()
    })
    .await;
    let tracker = lifecycle.tracker("test", AlertSurface::Channel);
    let announced = line();
    repository.fail_next_prune.store(true, Ordering::SeqCst);
    assert!(
        tracker
            .mark_taken(&[slot(&announced)], |_| async {
                panic!("no plan should reach the adapter")
            })
            .await
            .is_err()
    );
    assert_eq!(repository.prunes.load(Ordering::SeqCst), 1);
    assert!(
        tracker
            .mark_taken(&[slot(&announced)], |_| async {
                panic!("no plan should reach the adapter")
            })
            .await
            .is_err()
    );
    assert_eq!(repository.prunes.load(Ordering::SeqCst), 2);
    assert!(
        tracker
            .mark_taken(&[slot(&announced)], |_| async {
                panic!("no plan should reach the adapter")
            })
            .await
            .is_err()
    );
    assert_eq!(repository.prunes.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn edits_precede_pruning_and_the_grace_keeps_other_surfaces_available() {
    let (lifecycle, repository) = lifecycle(Faults::default()).await;
    let channel = lifecycle.tracker("test", AlertSurface::Channel);
    let dm = lifecycle.tracker("test", AlertSurface::DirectMessage);
    let mut old = line();
    old.starts_at = Utc::now() - Duration::hours(3);
    old.ends_at = old.starts_at + Duration::hours(1);
    let mut recent = line();
    recent.starts_at = Utc::now() - Duration::minutes(45);
    recent.ends_at = recent.starts_at + Duration::minutes(30);
    channel
        .record(None, "old", std::slice::from_ref(&old))
        .await;
    dm.record(Some("room"), "recent", std::slice::from_ref(&recent))
        .await;
    let edits = AtomicUsize::new(0);
    channel
        .mark_taken(&[slot(&old)], |_| async {
            assert_eq!(repository.prunes.load(Ordering::SeqCst), 0);
            edits.fetch_add(1, Ordering::SeqCst);
            Ok(EditOutcome::Edited)
        })
        .await
        .unwrap();
    dm.mark_taken(&[slot(&recent)], |_| async {
        edits.fetch_add(1, Ordering::SeqCst);
        Ok(EditOutcome::Edited)
    })
    .await
    .unwrap();
    assert_eq!(edits.load(Ordering::SeqCst), 2);
    assert_eq!(repository.prunes.load(Ordering::SeqCst), 1);
    assert_eq!(
        repository
            .store
            .prune_ended(Utc::now() + Duration::days(1))
            .await
            .unwrap(),
        1
    );
}
