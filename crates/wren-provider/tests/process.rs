use wren_provider::{ProviderRequest, ProviderResponse, ProviderSupervisor};

#[test]
fn provider_is_a_restartable_process_failure_boundary() {
    let executable = env!("CARGO_BIN_EXE_wren-client-providers");
    let mut supervisor = ProviderSupervisor::spawn(executable).expect("spawn provider");
    assert_eq!(
        supervisor
            .request(&ProviderRequest::Hello { protocol: 1 })
            .expect("hello"),
        ProviderResponse::Hello { protocol: 1 }
    );
    assert!(supervisor.request(&ProviderRequest::CrashForTest).is_err());
    assert_eq!(supervisor.restart_count(), 1);
    assert_eq!(
        supervisor
            .request(&ProviderRequest::Hello { protocol: 1 })
            .expect("hello after restart"),
        ProviderResponse::Hello { protocol: 1 }
    );
}
