//! Run through scripts/test-spanner.py against an isolated official emulator.
use scion_store::{ImmutableRecord, MemoryStore, SharedStore, Storage};
use scion_store_spanner::{SpannerConfig, SpannerStore};
use scion_types::{Error, ObjectId, ObjectKind};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[test]
fn spanner_metadata_contract() {
    let Ok(database) = std::env::var("SCION_TEST_SPANNER_DATABASE") else {
        eprintln!(
            "Set SCION_TEST_SPANNER_DATABASE and SPANNER_EMULATOR_HOST to run the emulator contract"
        );
        return;
    };
    let objects: SharedStore = Arc::new(MemoryStore::default());
    let first =
        Arc::new(SpannerStore::connect(objects.clone(), SpannerConfig::new(&database)).unwrap());
    let second =
        Arc::new(SpannerStore::connect(objects.clone(), SpannerConfig::new(&database)).unwrap());
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let namespace = format!("contract-{unique}");

    // Two independent clients observe immutable idempotence and typed conflicts.
    assert_eq!(first.get_record(&namespace, "immutable").unwrap(), None);
    first.put_record(&namespace, "immutable", b"one").unwrap();
    second.put_record(&namespace, "immutable", b"one").unwrap();
    assert!(matches!(
        second.put_record(&namespace, "immutable", b"two"),
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        second.get_record(&namespace, "immutable").unwrap(),
        Some(b"one".to_vec())
    );

    let wins = thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let store = if index % 2 == 0 { &first } else { &second };
                let namespace = &namespace;
                scope.spawn(move || {
                    store
                        .compare_and_swap(namespace, "ref", None, Some(&[index]))
                        .unwrap()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| usize::from(handle.join().unwrap()))
            .sum::<usize>()
    });
    assert_eq!(wins, 1);
    let current = first.get_record(&namespace, "ref").unwrap().unwrap();
    assert!(
        !first
            .compare_and_swap(&namespace, "ref", Some(b"wrong"), None)
            .unwrap()
    );
    assert!(
        second
            .compare_and_swap(&namespace, "ref", Some(&current), None)
            .unwrap()
    );
    assert_eq!(first.get_record(&namespace, "ref").unwrap(), None);
    assert!(
        first
            .compare_and_swap(&namespace, "absent", None, None)
            .unwrap()
    );

    for key in ["prefix/β", "prefix/a", "prefix/é", "other"] {
        first.put_record(&namespace, key, key.as_bytes()).unwrap();
    }
    let scanned = second.scan_records(&namespace, "prefix/", 10).unwrap();
    assert_eq!(
        second
            .scan_records(&namespace, "prefix/", 1_000_000)
            .unwrap(),
        scanned
    );
    assert_eq!(
        scanned
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["prefix/a", "prefix/é", "prefix/β"]
    );
    assert_eq!(
        first.scan_records(&namespace, "prefix/", 1).unwrap().len(),
        1
    );

    // A conflict after an earlier key was buffered must roll back the entire
    // transaction, including managed membership and the sequence counter.
    let repo = format!("rollback-{unique}");
    let id = ObjectId::hash(ObjectKind::Changeset, b"must roll back");
    first.put_record(&namespace, "z-existing", b"old").unwrap();
    let records = [
        ImmutableRecord {
            namespace: namespace.clone(),
            key: "a-prepared".into(),
            value: b"new".to_vec(),
        },
        ImmutableRecord {
            namespace: namespace.clone(),
            key: "z-existing".into(),
            value: b"different".to_vec(),
        },
    ];
    assert!(matches!(
        first.publish_commit(&repo, id, &records),
        Err(Error::Conflict(_))
    ));
    assert_eq!(second.get_record(&namespace, "a-prepared").unwrap(), None);
    assert_eq!(
        second
            .get_record(&format!("repo/{repo}/changesets"), &id.to_string())
            .unwrap(),
        None
    );
    assert!(second.read_log(&repo, 0, 10).unwrap().is_empty());
    let valid = [records[0].clone()];
    assert_eq!(first.publish_commit(&repo, id, &valid).unwrap(), 1);
    assert_eq!(second.publish_commit(&repo, id, &valid).unwrap(), 1);
    assert_eq!(second.append_log(&repo, id).unwrap(), 1);
    assert_eq!(
        second.get_record(&namespace, "a-prepared").unwrap(),
        Some(b"new".to_vec())
    );
    assert_eq!(
        second
            .get_record(&format!("repo/{repo}/changesets"), &id.to_string())
            .unwrap(),
        Some(b"1".to_vec())
    );

    // Polling while concurrent publishers commit must never skip a lower
    // sequence that becomes visible after the consumer advances its cursor.
    let repo = format!("frontier-{unique}");
    let ids: Vec<_> = (0..12)
        .map(|i| ObjectId::hash(ObjectKind::Changeset, format!("{unique}-{i}").as_bytes()))
        .collect();
    let finished = AtomicBool::new(false);
    let seen = thread::scope(|scope| {
        let consumer = scope.spawn(|| {
            let mut cursor = 0;
            let mut observed = Vec::new();
            loop {
                let batch = second.read_log(&repo, cursor, 3).unwrap();
                if batch.is_empty() {
                    if finished.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                for entry in batch {
                    assert_eq!(entry.sequence, cursor + 1);
                    cursor = entry.sequence;
                    observed.push(entry.id);
                }
            }
            observed
        });
        let writers: Vec<_> = ids
            .iter()
            .map(|id| {
                scope.spawn(|| {
                    let sequence = first.append_log(&repo, *id).unwrap();
                    assert_eq!(first.append_log(&repo, *id).unwrap(), sequence);
                })
            })
            .collect();
        let outcomes: Vec<_> = writers.into_iter().map(|writer| writer.join()).collect();
        finished.store(true, Ordering::Release);
        let observed = consumer.join().unwrap();
        for outcome in outcomes {
            outcome.unwrap();
        }
        observed
    });
    assert_eq!(seen.len(), ids.len());
    assert_eq!(
        seen.into_iter().collect::<BTreeSet<_>>(),
        ids.into_iter().collect()
    );
    assert!(first.read_log(&repo, u64::MAX, 1).unwrap().is_empty());

    let blob = first
        .put_object(ObjectKind::Blob, b"objects use the delegated backend")
        .unwrap();
    assert_eq!(
        objects.get_object(ObjectKind::Blob, blob).unwrap(),
        b"objects use the delegated backend"
    );
    let reopened = SpannerStore::connect(objects, SpannerConfig::new(database)).unwrap();
    assert_eq!(
        reopened.get_record(&namespace, "immutable").unwrap(),
        Some(b"one".to_vec())
    );
    assert_eq!(
        reopened.get_object(ObjectKind::Blob, blob).unwrap(),
        b"objects use the delegated backend"
    );
}
