use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use wren_provider::{AccelerationBackend, ProviderActor, ProviderRequest, ProviderResponse};
use wren_types::{DocumentId, DocumentRevision, LanguageBundle, Priority, ProviderDemand};

const MIB: usize = 1024 * 1024;
const WORKLOADS: [(&str, usize); 3] =
    [("4-mib", 4 * MIB), ("8-mib", 8 * MIB), ("32-mib", 32 * MIB)];

fn bundle() -> LanguageBundle {
    LanguageBundle {
        language_id: "generated-provider-input".into(),
        grammar_hash: [1; 32],
        grammar_abi: 15,
        grammar_semver: "0.24".into(),
        highlight_query_hash: [2; 32],
        object_query_hash: [3; 32],
        outline_query_hash: [4; 32],
        injection_query_hash: [5; 32],
        config_schema_version: 1,
    }
}

fn generated_source(bytes: usize) -> Box<str> {
    let mut source = String::with_capacity(bytes);
    let mut block = String::with_capacity(4 * 1024);
    block.push_str("pub fn generated_provider_item() { let result = return_value; }\n");
    while block.len() < 4 * 1024 {
        block.push_str("generated_identifier_0123456789abcdef = another_identifier;\n");
    }
    block.truncate(4 * 1024);
    while source.len() + block.len() <= bytes {
        source.push_str(&block);
    }
    source.push_str(&block[..bytes - source.len()]);
    source.into_boxed_str()
}

fn demand(document_id: DocumentId, bytes: usize) -> ProviderRequest {
    ProviderRequest::Demand {
        document_id,
        demand: ProviderDemand {
            revision: DocumentRevision::new(1),
            visible: std::iter::once(0..bytes).collect(),
            near_viewport: Vec::new(),
            priority: Priority::Visible,
        },
    }
}

fn update(actor: &mut ProviderActor, document_id: DocumentId, source: Box<str>) {
    actor
        .handle(ProviderRequest::UpdateDocument {
            document_id,
            revision: DocumentRevision::new(1),
            text: source,
            bundle: bundle(),
        })
        .expect("load provider benchmark document");
}

fn highlight(actor: &mut ProviderActor, document_id: DocumentId, bytes: usize) -> ProviderResponse {
    actor
        .handle(demand(document_id, bytes))
        .expect("highlight provider benchmark document")
}

fn provider_acceleration(criterion: &mut Criterion) {
    let mut cpu = ProviderActor::cpu_only();
    let mut gpu = ProviderActor::default();
    for (index, (_, bytes)) in WORKLOADS.into_iter().enumerate() {
        let document_id = DocumentId::new(u64::try_from(index + 1).unwrap_or(u64::MAX));
        let source = generated_source(bytes);
        update(&mut cpu, document_id, source.clone());
        update(&mut gpu, document_id, source);
    }

    let (first_name, first_bytes) = WORKLOADS[0];
    let first_document = DocumentId::new(1);
    let expected = highlight(&mut cpu, first_document, first_bytes);
    let actual = highlight(&mut gpu, first_document, first_bytes);
    assert_eq!(actual, expected, "GPU and CPU provider output diverged");
    let gpu_available = gpu.acceleration_backend() == AccelerationBackend::Gpu;
    if !gpu_available {
        eprintln!(
            "hardware GPU unavailable; recording CPU baselines only (first workload: {first_name})"
        );
    }

    let mut group = criterion.benchmark_group("provider-lexical-classification");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(5));
    for (index, (name, bytes)) in WORKLOADS.into_iter().enumerate() {
        let document_id = DocumentId::new(u64::try_from(index + 1).unwrap_or(u64::MAX));
        group.throughput(Throughput::Bytes(u64::try_from(bytes).unwrap_or(u64::MAX)));
        group.bench_with_input(BenchmarkId::new("cpu", name), &bytes, |bencher, bytes| {
            bencher.iter(|| black_box(highlight(&mut cpu, document_id, *bytes)));
        });
        if gpu_available {
            let expected = highlight(&mut cpu, document_id, bytes);
            let actual = highlight(&mut gpu, document_id, bytes);
            assert_eq!(actual, expected, "GPU and CPU provider output diverged");
            group.bench_with_input(BenchmarkId::new("gpu", name), &bytes, |bencher, bytes| {
                bencher.iter(|| black_box(highlight(&mut gpu, document_id, *bytes)));
            });
        }
    }
    group.finish();
}

criterion_group!(benches, provider_acceleration);
criterion_main!(benches);
