#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use wren_remote::{OpenSshSpec, RemoteWorkspaceClient};
use wren_types::{
    ClientId, ClientMutation, ClientSequence, DocumentId, DocumentMutation, DocumentRevision, Edit,
    LeaseEpoch, MutationId, MutationResult, SaveRequest, SemanticGroupId, SemanticGroupKind,
    SessionId, SessionSequence, Transaction, WorkspaceGeneration,
};

#[test]
fn real_dual_stdio_agent_opens_mutates_saves_and_searches() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let workspace = temporary.path().join("workspace");
    let state = temporary.path().join("state");
    fs::create_dir(&workspace).expect("workspace directory");
    fs::write(workspace.join("main.rs"), "fn main() {}\n").expect("seed source");
    fs::write(workspace.join("other.rs"), "fn other() {}\n").expect("second source");

    let fake_ssh = temporary.path().join("ssh");
    fs::write(
        &fake_ssh,
        "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    -T) shift ;;\n    -o|-p|-i) shift 2 ;;\n    --) shift ;;\n    *) shift; break ;;\n  esac\ndone\nexec \"$@\"\n",
    )
    .expect("write fake ssh");
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o700))
        .expect("make fake ssh executable");

    let spec = OpenSshSpec {
        executable: fake_ssh,
        host: "test-host".into(),
        user: None,
        port: None,
        identity_file: None,
        extra_options: Vec::new(),
        remote_session_program: env!("CARGO_BIN_EXE_wren-sessiond").into(),
        remote_workspace: Some(workspace.clone()),
        remote_state_dir: Some(state),
    };
    let mut client = RemoteWorkspaceClient::connect(&spec).expect("connect both lanes");
    let manifest = client
        .manifest(WorkspaceGeneration::new(1))
        .expect("remote manifest");
    assert!(manifest.entries.contains_key("main.rs"));

    let opened = client
        .open(DocumentId::new(9), ClientId::new(7), "main.rs", None)
        .expect("remote open");
    assert!(!opened.cached_hash_valid);
    assert_eq!(
        client.blob(opened.content_hash).expect("bulk blob"),
        b"fn main() {}\n"
    );
    let cached = client
        .open(
            DocumentId::new(9),
            ClientId::new(7),
            "main.rs",
            Some(opened.content_hash),
        )
        .expect("cached reopen");
    assert!(cached.cached_hash_valid);
    assert!(matches!(
        client.open(DocumentId::new(9), ClientId::new(7), "other.rs", None),
        Err(wren_remote::RemoteError::Peer(_))
    ));

    let mutation = ClientMutation {
        mutation_id: MutationId::new(1),
        client_id: ClientId::new(7),
        client_sequence: ClientSequence::new(1),
        state_deltas: Vec::new(),
        documents: vec![DocumentMutation {
            document_id: DocumentId::new(9),
            lease_epoch: LeaseEpoch::new(1),
            base_revision: DocumentRevision::new(0),
            semantic_group_id: SemanticGroupId::new(1),
            semantic_group_kind: SemanticGroupKind::InsertRun,
            undo_parent: None,
            transactions: vec![
                Transaction::new(
                    DocumentRevision::new(0),
                    vec![Edit::new(13..13, "// remote\n")],
                )
                .expect("transaction"),
            ],
        }],
    };
    let MutationResult::Durable {
        session_sequence, ..
    } = client.submit(&mutation).expect("durable mutation")
    else {
        panic!("mutation was not durable");
    };
    let saved = client
        .save(&SaveRequest {
            document_id: DocumentId::new(9),
            required_frontier: DocumentRevision::new(1),
            expected_file_identity: opened.file_identity,
            expected_content_hash: opened.content_hash,
        })
        .expect("persist remote frontier");
    assert_eq!(saved.persisted_frontier, DocumentRevision::new(1));
    assert_eq!(
        fs::read_to_string(workspace.join("main.rs")).expect("saved source"),
        "fn main() {}\n// remote\n"
    );
    let hits = client.search("remote", 10).expect("bulk remote search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path.as_ref(), "main.rs");
    client.heartbeat(41).expect("application heartbeat");
    let resumed = client
        .reconnect(&wren_types::Resume {
            session_id: SessionId::new(1),
            session_epoch: opened.session_epoch,
            last_session_sequence: session_sequence,
            document_frontiers: vec![wren_types::DocumentFrontier {
                document_id: DocumentId::new(9),
                revision: DocumentRevision::new(1),
            }],
            outstanding_mutation_ids: Vec::new(),
        })
        .expect("application-level resume over replacement lanes");
    assert!(matches!(
        resumed,
        wren_types::ResumeResult::Replay { .. } | wren_types::ResumeResult::SnapshotRequired { .. }
    ));
    client
        .heartbeat(SessionSequence::new(42).get())
        .expect("heartbeat after reconnect");
    client.close().expect("close both lanes");
}
