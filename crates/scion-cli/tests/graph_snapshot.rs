use scion_engine::Repository;
use scion_store::LocalStore;
use scion_types::{Changeset, ObjectId, ObjectKind};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Command, Output},
    sync::Arc,
};

fn request(parents: &[ObjectId], message: &str) -> Changeset {
    Changeset {
        parents: parents.to_vec(),
        author: "CLI test".into(),
        committer: None,
        timestamp_ms: 0,
        message: message.into(),
        changes: BTreeMap::new(),
        metadata: BTreeMap::new(),
    }
}

fn scn(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scn"))
        .current_dir(directory)
        .env_remove("SCION_REPO")
        .env_remove("SCION_OBJECTS")
        .env_remove("SCION_TOKEN")
        .env("SCION_SERVER", "http://127.0.0.1:1")
        // Tokio rejects this worker count if a multithread runtime is built.
        // Local graph commands must work without consulting runtime settings.
        .env("TOKIO_WORKER_THREADS", "0")
        .args(arguments)
        .output()
        .unwrap()
}

fn value(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn local_frozen_commands_create_inspect_and_query_without_a_server() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(temporary.path().join("data")).unwrap());
    let repository = Repository::new("example", store).unwrap();
    let root = repository
        .create_commit(request(&[], "root"))
        .unwrap()
        .id()
        .unwrap();
    let left = repository
        .create_commit(request(&[root], "left"))
        .unwrap()
        .id()
        .unwrap();
    let right = repository
        .create_commit(request(&[root], "right"))
        .unwrap()
        .id()
        .unwrap();
    let created = value(scn(
        temporary.path(),
        &[
            "--json",
            "--repo",
            "example",
            "graph-snapshot",
            "create",
            "graph.scng",
            "--data",
            "data",
        ],
    ));
    assert_eq!(created["repository"], "example");
    assert_eq!(created["node_count"], 3);
    assert_eq!(created["source"]["sequence"], 3);
    let identity = created["source"]["identity"].as_str().unwrap();
    assert_eq!(
        value(scn(
            temporary.path(),
            &["--json", "graph-snapshot", "info", "graph.scng"]
        )),
        created
    );
    let root_text = root.to_string();
    let left_text = left.to_string();
    let right_text = right.to_string();
    assert_eq!(
        value(scn(
            temporary.path(),
            &[
                "--json",
                "--repo",
                "example",
                "graph-snapshot",
                "ancestor",
                "graph.scng",
                &root_text,
                &left_text,
                "--identity",
                identity
            ]
        )),
        true
    );
    assert_eq!(
        value(scn(
            temporary.path(),
            &[
                "--json",
                "--repo",
                "example",
                "graph-snapshot",
                "ancestor",
                "graph.scng",
                &left_text,
                &right_text
            ]
        )),
        false
    );
    assert_eq!(
        value(scn(
            temporary.path(),
            &[
                "--json",
                "--repo",
                "example",
                "graph-snapshot",
                "merge-base",
                "graph.scng",
                &left_text,
                &right_text
            ]
        )),
        serde_json::json!([root])
    );
    assert_eq!(
        value(scn(
            temporary.path(),
            &[
                "--json",
                "--repo",
                "example",
                "graph-snapshot",
                "nth-parent",
                "graph.scng",
                &left_text,
                "1"
            ]
        )),
        root_text
    );
    assert!(
        value(scn(
            temporary.path(),
            &[
                "--json",
                "--repo",
                "example",
                "graph-snapshot",
                "nth-parent",
                "graph.scng",
                &left_text,
                "2"
            ]
        ))
        .is_null()
    );

    let later = repository
        .create_commit(request(&[left], "later"))
        .unwrap()
        .id()
        .unwrap();
    let missing = scn(
        temporary.path(),
        &[
            "--json",
            "--repo",
            "example",
            "graph-snapshot",
            "ancestor",
            "graph.scng",
            &root_text,
            &later.to_string(),
        ],
    );
    assert_eq!(missing.status.code(), Some(4));
    let refreshed = value(scn(
        temporary.path(),
        &[
            "--json",
            "--repo",
            "example",
            "graph-snapshot",
            "create",
            "graph.scng",
            "--data",
            "data",
        ],
    ));
    assert_eq!(refreshed["node_count"], 4);
    assert_ne!(
        refreshed["source"]["identity"],
        created["source"]["identity"]
    );
    assert!(
        !scn(
            temporary.path(),
            &[
                "--json",
                "--repo",
                "example",
                "graph-snapshot",
                "ancestor",
                "graph.scng",
                &root_text,
                &left_text,
                "--identity",
                identity
            ]
        )
        .status
        .success()
    );
}

#[test]
fn frozen_cli_requires_explicit_scope_full_ids_and_safe_outputs() {
    let temporary = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(temporary.path().join("data")).unwrap());
    let repository = Repository::new("example", store).unwrap();
    let root = repository
        .create_commit(request(&[], "root"))
        .unwrap()
        .id()
        .unwrap();
    let root_text = root.to_string();
    value(scn(
        temporary.path(),
        &[
            "--json",
            "--repo",
            "example",
            "graph-snapshot",
            "create",
            "graph.scng",
            "--data",
            "data",
        ],
    ));
    let before = fs::read(temporary.path().join("graph.scng")).unwrap();
    let unscoped = scn(
        temporary.path(),
        &[
            "--json",
            "graph-snapshot",
            "ancestor",
            "graph.scng",
            &root_text,
            &root_text,
        ],
    );
    assert!(!unscoped.status.success());
    assert!(String::from_utf8_lossy(&unscoped.stderr).contains("--repo"));
    assert!(
        !scn(
            temporary.path(),
            &[
                "--json",
                "--repo",
                "foreign",
                "graph-snapshot",
                "ancestor",
                "graph.scng",
                &root_text,
                &root_text
            ]
        )
        .status
        .success()
    );
    assert!(
        !scn(
            temporary.path(),
            &[
                "--json",
                "--repo",
                "example",
                "graph-snapshot",
                "ancestor",
                "graph.scng",
                "main",
                &root_text
            ]
        )
        .status
        .success()
    );
    let missing = ObjectId::hash(ObjectKind::Changeset, b"absent").to_string();
    assert_eq!(
        scn(
            temporary.path(),
            &[
                "--json",
                "--repo",
                "example",
                "graph-snapshot",
                "nth-parent",
                "graph.scng",
                &missing,
                "0"
            ]
        )
        .status
        .code(),
        Some(4)
    );
    assert!(
        !scn(
            temporary.path(),
            &[
                "--json",
                "--repo",
                "example",
                "graph-snapshot",
                "create",
                "graph.scng",
                "--data",
                "data",
                "--max-commits",
                "0"
            ]
        )
        .status
        .success()
    );
    assert_eq!(
        fs::read(temporary.path().join("graph.scng")).unwrap(),
        before
    );
    assert!(
        !scn(
            temporary.path(),
            &[
                "--json",
                "--repo",
                "example",
                "graph-snapshot",
                "create",
                "data/metadata.sqlite3",
                "--data",
                "data"
            ]
        )
        .status
        .success()
    );
    assert!(repository.get_changeset(root).is_ok());
}
