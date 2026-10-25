use scion_client::{Client, ObjectCache, StatusKind, WorkingCopy};
use scion_engine::Repository;
use scion_protocol::{ObjectRequest, ObjectWitness, RpcRequest};
use scion_server::{
    ServerConfig, ServerState,
    auth::{PathRule, Permissions, Policy, Principal},
    router,
};
use scion_store::{MemoryStore, SharedStore};
use scion_types::*;
use std::{collections::BTreeMap, sync::Arc};

struct Service {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Service {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn start(store: SharedStore, config: ServerConfig) -> Service {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = router(ServerState::new(store, config).unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Service { url, task }
}
fn client(s: &Service, repo: &str, token: Option<&str>) -> Client {
    Client::new(&s.url, None, repo, token.map(str::to_owned), None).unwrap()
}
fn dev() -> ServerConfig {
    ServerConfig {
        development: true,
        requests_per_second: 10_000,
        burst: 20_000,
        ..ServerConfig::default()
    }
}
async fn commit(
    c: &Client,
    parent: Option<ObjectId>,
    files: &[(&str, &[u8])],
    message: &str,
) -> ObjectId {
    let mut changes = BTreeMap::new();
    for (path, bytes) in files {
        let id = c.put_blob(bytes).await.unwrap();
        changes.insert(
            RepoPath::new(*path).unwrap(),
            FileChange::put(FileEntry {
                id,
                size: bytes.len() as u64,
                kind: FileKind::Regular,
            }),
        );
    }
    c.create_commit(Changeset {
        parents: parent.into_iter().collect(),
        author: "Test <test@example.org>".into(),
        committer: None,
        timestamp_ms: 1,
        message: message.into(),
        changes,
        metadata: BTreeMap::new(),
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn independent_replicas_create_commits_merge_and_cas_without_a_working_copy() {
    let store: SharedStore = Arc::new(MemoryStore::new());
    let s1 = start(store.clone(), dev()).await;
    let s2 = start(store, dev()).await;
    let c1 = client(&s1, "demo", None);
    let c2 = client(&s2, "demo", None);
    let base = commit(&c1, None, &[("src/main.rs", b"one\ntwo\nthree\n")], "base").await;
    let (a, b) = tokio::join!(
        commit(
            &c1,
            Some(base),
            &[("src/main.rs", b"ONE\ntwo\nthree\n")],
            "left"
        ),
        commit(
            &c2,
            Some(base),
            &[("src/main.rs", b"one\ntwo\nTHREE\n")],
            "right"
        )
    );
    let refs: BTreeMap<String, ObjectId> = c1.rpc(RpcRequest::ListRefs).await.unwrap();
    assert!(refs.is_empty());
    let result: MergeResult = c1
        .rpc(RpcRequest::Merge {
            ours: a,
            theirs: b,
            author: "Test".into(),
            message: "merge".into(),
            timestamp_ms: 2,
        })
        .await
        .unwrap();
    let merged = result.changeset.unwrap();
    assert!(result.conflicts.is_empty());
    assert_eq!(
        c2.read_file(merged, RepoPath::new("src/main.rs").unwrap())
            .await
            .unwrap(),
        b"ONE\ntwo\nTHREE\n"
    );
    let cas1: bool = c1
        .rpc(RpcRequest::SetRef {
            name: "main".into(),
            expected: None,
            target: Some(merged),
        })
        .await
        .unwrap();
    assert!(cas1);
    let stale: bool = c2
        .rpc(RpcRequest::SetRef {
            name: "main".into(),
            expected: None,
            target: Some(a),
        })
        .await
        .unwrap();
    assert!(!stale);
    let bases: Vec<ObjectId> = c2.rpc(RpcRequest::MergeBases { a, b }).await.unwrap();
    assert_eq!(bases, vec![base]);
    let reachable: bool = c1
        .rpc(RpcRequest::IsAncestor {
            ancestor: b,
            descendant: merged,
        })
        .await
        .unwrap();
    assert!(reachable);
    let blame: Vec<BlameLine> = c1
        .rpc(RpcRequest::Blame {
            revision: merged,
            path: RepoPath::new("src/main.rs").unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(blame.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_lazy_working_copy_never_interprets_absent_files_as_deletions() {
    let s = start(Arc::new(MemoryStore::new()), dev()).await;
    let c = client(&s, "demo", None);
    let base = commit(
        &c,
        None,
        &[
            ("src/main.rs", b"hello\n"),
            ("src/other.rs", b"keep\n"),
            ("docs/readme.md", b"hidden by sparse\n"),
        ],
        "base",
    )
    .await;
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("wc");
    let mut wc = WorkingCopy::clone(&c, base, &dest, vec![RepoPath::new("src").unwrap()], true)
        .await
        .unwrap();
    assert!(wc.status().unwrap().is_empty());
    assert!(!dest.join("src/main.rs").exists());
    assert_eq!(wc.state().files.len(), 2);
    assert_eq!(
        wc.hydrate(&c, &RepoPath::new("src/main.rs").unwrap())
            .await
            .unwrap(),
        1
    );
    std::fs::write(dest.join("src/main.rs"), b"changed\n").unwrap();
    let status = wc.status().unwrap();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].status, StatusKind::Modified);
    let next = wc
        .commit(&c, "Test".into(), "sparse edit".into(), None)
        .await
        .unwrap();
    assert_eq!(
        c.read_file(next, RepoPath::new("src/other.rs").unwrap())
            .await
            .unwrap(),
        b"keep\n"
    );
    assert_eq!(
        c.read_file(next, RepoPath::new("docs/readme.md").unwrap())
            .await
            .unwrap(),
        b"hidden by sparse\n"
    );
    std::fs::write(dest.join("src/main.rs"), b"dirty\n").unwrap();
    assert!(wc.checkout(&c, base, false).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn capabilities_survive_cache_tiers_without_bypassing_authorization() {
    use scion_cache::CacheService;
    use scion_protocol::{ObjectTicket, Tier};
    let s = start(Arc::new(MemoryStore::new()), dev()).await;
    let c = client(&s, "demo", None);
    let bytes = b"immutable cached payload";
    let base = commit(&c, None, &[("file", bytes)], "base").await;
    let id = ObjectId::hash(ObjectKind::Blob, bytes);
    let ticket = c
        .object_ticket(ObjectWitness {
            commit: base,
            path: RepoPath::new("file").unwrap(),
            kind: ObjectKind::Blob,
            id,
        })
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cache = CacheService::connect(
        ObjectCache::open(tmp.path().join("cache"), 1024 * 1024).unwrap(),
        &s.url,
        Tier::Cache,
        [0; 32],
        8,
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cache_url = format!("http://{}", listener.local_addr().unwrap());
    let app = cache.router();
    let cache_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let daemon = CacheService::connect(
        ObjectCache::open(tmp.path().join("daemon"), 1024 * 1024).unwrap(),
        cache_url,
        Tier::Daemon,
        [0; 32],
        8,
    )
    .await
    .unwrap();
    assert!(
        CacheService::connect(
            ObjectCache::open(tmp.path().join("invalid"), 1024).unwrap(),
            &s.url,
            Tier::Daemon,
            [0; 32],
            8
        )
        .await
        .is_err()
    );
    let first = daemon
        .fetch(
            "demo",
            ObjectRequest {
                ticket: ticket.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        scion_store::decode_object(ObjectKind::Blob, id, &first).unwrap(),
        bytes
    );
    drop(s);
    cache_task.abort();
    assert!(
        daemon
            .fetch(
                "demo",
                ObjectRequest {
                    ticket: ticket.clone()
                }
            )
            .await
            .is_ok()
    );
    let forged = ObjectTicket {
        repo: "another".into(),
        ..ticket.clone()
    };
    assert!(
        daemon
            .fetch("another", ObjectRequest { ticket: forged })
            .await
            .is_err()
    );
    let mut expired = ticket;
    expired.expires_ms = 0;
    expired.sign(&[0; 32]).unwrap();
    assert!(
        daemon
            .fetch("demo", ObjectRequest { ticket: expired })
            .await
            .is_err()
    );
    assert_eq!(
        daemon
            .metrics()
            .hits
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn directory_permissions_cover_metadata_object_paths_and_blob_provenance() {
    let store: SharedStore = Arc::new(MemoryStore::new());
    let r = Repository::new("demo", store.clone()).unwrap();
    let public = r.put_blob(b"public").unwrap();
    let secret = r.put_blob(b"sensitive").unwrap();
    let base = r
        .create_commit(Changeset {
            parents: vec![],
            author: "Admin".into(),
            committer: None,
            timestamp_ms: 1,
            message: "private names must not leak".into(),
            changes: BTreeMap::from([
                (
                    RepoPath::new("public.txt").unwrap(),
                    FileChange::put(FileEntry {
                        id: public,
                        size: 6,
                        kind: FileKind::Regular,
                    }),
                ),
                (
                    RepoPath::new("secret/password").unwrap(),
                    FileChange::put(FileEntry {
                        id: secret,
                        size: 9,
                        kind: FileKind::Regular,
                    }),
                ),
            ]),
            metadata: BTreeMap::new(),
        })
        .unwrap()
        .id()
        .unwrap();
    let token = "test-principal-long-token";
    let limited = Permissions {
        read: true,
        write: true,
        admin: false,
        rules: vec![PathRule {
            prefix: RepoPath::new("secret").unwrap(),
            read: Some(false),
            write: Some(false),
        }],
    };
    let policy = Policy {
        principals: vec![
            Principal {
                name: "alice".into(),
                token_hash: blake3::hash(token.as_bytes()).to_hex().to_string(),
                repositories: BTreeMap::from([("demo".into(), limited)]),
            },
            Principal {
                name: "reader".into(),
                token_hash: blake3::hash(b"reader-token-for-test").to_hex().to_string(),
                repositories: BTreeMap::from([(
                    "demo".into(),
                    Permissions {
                        read: true,
                        ..Permissions::default()
                    },
                )]),
            },
        ],
    };
    let s = start(
        store,
        ServerConfig {
            policy,
            capability_key: [7; 32],
            ..ServerConfig::default()
        },
    )
    .await;
    let c = client(&s, "demo", Some(token));
    assert_eq!(
        c.read_file(base, RepoPath::new("public.txt").unwrap())
            .await
            .unwrap(),
        b"public"
    );
    assert!(
        c.rpc::<Changeset>(RpcRequest::GetChangeset { id: base })
            .await
            .is_err()
    );
    assert!(
        c.rpc::<ObjectId>(RpcRequest::RootTree { commit: base })
            .await
            .is_err()
    );
    assert!(
        c.read_file(base, RepoPath::new("secret/password").unwrap())
            .await
            .is_err()
    );
    assert!(
        c.object_ticket(ObjectWitness {
            commit: base,
            path: RepoPath::new("public.txt").unwrap(),
            kind: ObjectKind::Blob,
            id: secret
        })
        .await
        .is_err()
    );
    let listed: BTreeMap<RepoPath, FileEntry> = c
        .rpc(RpcRequest::ListFiles {
            commit: base,
            prefix: RepoPath::root(),
        })
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    let forged = Changeset {
        parents: vec![base],
        author: "Alice".into(),
        committer: None,
        timestamp_ms: 2,
        message: "try to smuggle blob".into(),
        changes: BTreeMap::from([(
            RepoPath::new("leak").unwrap(),
            FileChange::put(FileEntry {
                id: secret,
                size: 9,
                kind: FileKind::Regular,
            }),
        )]),
        metadata: BTreeMap::new(),
    };
    assert!(c.create_commit(forged).await.is_err());
    let reader = client(&s, "demo", Some("reader-token-for-test"));
    let empty = Changeset {
        parents: vec![base],
        author: "Reader".into(),
        committer: None,
        timestamp_ms: 3,
        message: "unauthorized empty commit".into(),
        changes: BTreeMap::new(),
        metadata: BTreeMap::new(),
    };
    assert!(reader.create_commit(empty).await.is_err());
    let anonymous = client(&s, "demo", None);
    assert!(
        anonymous
            .rpc::<BTreeMap<String, ObjectId>>(RpcRequest::ListRefs)
            .await
            .is_err()
    );
}
