use wren_provider::{ProviderRequest, ProviderResponse, ProviderSupervisor};
use wren_types::{DocumentId, DocumentRevision, LanguageBundle, Priority, ProviderDemand};

fn language_bundle(language_id: &str) -> LanguageBundle {
    let mut identity = [0_u8; 32];
    for (index, byte) in language_id.bytes().enumerate() {
        identity[index % identity.len()] ^= byte;
    }
    LanguageBundle {
        language_id: language_id.into(),
        grammar_hash: identity,
        grammar_abi: 15,
        grammar_semver: "bundled".into(),
        highlight_query_hash: identity,
        object_query_hash: identity,
        outline_query_hash: identity,
        injection_query_hash: identity,
        config_schema_version: 1,
    }
}

#[test]
fn provider_is_a_restartable_process_failure_boundary() {
    let executable = env!("CARGO_BIN_EXE_wren-client-providers");
    let mut supervisor = ProviderSupervisor::spawn(executable).expect("spawn provider");
    assert_eq!(supervisor.request(&ProviderRequest::Hello { protocol: 1 }).expect("hello"), ProviderResponse::Hello { protocol: 1 });
    assert!(supervisor.request(&ProviderRequest::CrashForTest).is_err());
    assert_eq!(supervisor.restart_count(), 1);
    assert_eq!(supervisor.request(&ProviderRequest::Hello { protocol: 1 }).expect("hello after restart"), ProviderResponse::Hello { protocol: 1 });
}

#[test]
fn provider_process_loads_nix_tree_sitter_without_runtime_installation() {
    let executable = env!("CARGO_BIN_EXE_wren-client-providers");
    let mut supervisor = ProviderSupervisor::spawn(executable).expect("spawn provider");
    let source = "{ lib, ... }: let greeting = \"hello\"; in { enabled = lib.mkDefault true; } # note\n";
    let document_id = DocumentId::new(7);
    let revision = DocumentRevision::new(0);
    assert!(matches!(
        supervisor
            .request(&ProviderRequest::UpdateDocument { document_id, revision, text: source.into(), bundle: language_bundle("nix") })
            .expect("load Nix document"),
        ProviderResponse::Updated { .. }
    ));
    let ProviderResponse::Highlight(highlight) = supervisor
        .request(&ProviderRequest::Demand {
            document_id,
            demand: ProviderDemand { revision, visible: std::iter::once(0..source.len()).collect(), near_viewport: Vec::new(), priority: Priority::Visible },
        })
        .expect("highlight Nix document")
    else {
        panic!("unexpected provider response");
    };
    for (needle, kind) in [("lib", "variable.parameter"), ("enabled", "variable.member"), ("mkDefault", "function.call")] {
        let start = source.find(needle).expect("Nix token");
        assert!(
            highlight.spans.iter().any(|span| { span.range == (start..start + needle.len()) && span.kind.as_ref() == kind }),
            "provider process did not classify {needle:?} as {kind}: {:?}",
            highlight.spans
        );
    }
}
