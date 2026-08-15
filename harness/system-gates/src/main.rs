use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hdrhistogram::Histogram;
use serde_json::{Value, json};
use wren_provider::{LatestDemandQueue, ProviderActor, ProviderRequest};
use wren_remote::{
    BlobCache, OpenSshSpec, RemoteMaterializer, RemoteWorkspaceClient, fastcdc_chunks,
};
use wren_text::{SnapshotManager, SnapshotQuota};
use wren_types::{
    ClientId, ClientMutation, ClientSequence, DocumentClass, DocumentId, DocumentMutation,
    DocumentRevision, Edit, LanguageBundle, MutationId, MutationResult, Priority, ProviderDemand,
    SaveRequest, SemanticGroupId, SemanticGroupKind, Transaction, WorkspaceGeneration,
};

#[derive(Debug)]
struct Arguments {
    iterations: u64,
    cpu: Option<usize>,
    output: Option<PathBuf>,
    gate: bool,
}

fn arguments() -> Result<Arguments> {
    let values = env::args().skip(1).collect::<Vec<_>>();
    let mut iterations = 1_000;
    let mut output = None;
    let mut cpu = None;
    let mut gate = false;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--iterations" => {
                index += 1;
                iterations = values
                    .get(index)
                    .context("--iterations needs a value")?
                    .parse()?;
            }
            "--output" => {
                index += 1;
                output = Some(PathBuf::from(
                    values.get(index).context("--output needs a value")?,
                ));
            }
            "--cpu" => {
                index += 1;
                cpu = Some(values.get(index).context("--cpu needs a value")?.parse()?);
            }
            "--gate" => gate = true,
            argument => anyhow::bail!("unknown argument: {argument}"),
        }
        index += 1;
    }
    anyhow::ensure!(iterations > 0, "--iterations must be positive");
    Ok(Arguments {
        iterations,
        cpu,
        output,
        gate,
    })
}

fn pin_cpu(index: usize) -> bool {
    core_affinity::get_core_ids()
        .and_then(|ids| ids.get(index).copied())
        .is_some_and(core_affinity::set_for_current)
}

fn validate_gate_environment(arguments: &Arguments, pinned: bool, has_remote: bool) -> Result<()> {
    if !arguments.gate {
        return Ok(());
    }
    anyhow::ensure!(
        env::var("WREN_BARE_METAL").as_deref() == Ok("1"),
        "--gate requires WREN_BARE_METAL=1 on the dedicated benchmark runner"
    );
    anyhow::ensure!(arguments.cpu.is_some(), "--gate requires --cpu");
    anyhow::ensure!(pinned, "requested benchmark CPU could not be pinned");
    anyhow::ensure!(
        env::var("WREN_NETEM_ACTIVE").as_deref() == Ok("1"),
        "--gate requires WREN_NETEM_ACTIVE=1 with tc netem installed"
    );
    anyhow::ensure!(
        has_remote,
        "--gate requires WREN_BENCH_REMOTE_HOST, WREN_BENCH_REMOTE_WORKSPACE, and WREN_BENCH_REMOTE_STATE"
    );
    Ok(())
}

fn remote_spec_from_environment() -> Option<OpenSshSpec> {
    let host = env::var("WREN_BENCH_REMOTE_HOST").ok()?;
    let workspace = env::var_os("WREN_BENCH_REMOTE_WORKSPACE").map(PathBuf::from)?;
    let state = env::var_os("WREN_BENCH_REMOTE_STATE").map(PathBuf::from)?;
    Some(OpenSshSpec {
        executable: env::var_os("WREN_BENCH_SSH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("ssh")),
        host: host.into_boxed_str(),
        user: env::var("WREN_BENCH_REMOTE_USER")
            .ok()
            .map(String::into_boxed_str),
        port: env::var("WREN_BENCH_REMOTE_PORT")
            .ok()
            .and_then(|value| value.parse().ok()),
        identity_file: env::var_os("WREN_BENCH_REMOTE_IDENTITY").map(PathBuf::from),
        extra_options: vec!["BatchMode=yes".into()],
        remote_session_program: env::var("WREN_BENCH_REMOTE_SESSIOND")
            .unwrap_or_else(|_| "wren-sessiond".to_owned())
            .into_boxed_str(),
        remote_workspace: Some(workspace),
        remote_state_dir: Some(state),
    })
}

fn histogram() -> Result<Histogram<u64>> {
    Histogram::new_with_bounds(1, 60_000_000_000, 3).map_err(Into::into)
}

fn distribution(histogram: &Histogram<u64>) -> Value {
    json!({
        "samples": histogram.len(),
        "min": histogram.min(),
        "p50": histogram.value_at_quantile(0.50),
        "p90": histogram.value_at_quantile(0.90),
        "p99": histogram.value_at_quantile(0.99),
        "max": histogram.max(),
    })
}

fn elapsed_nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn bundle() -> LanguageBundle {
    LanguageBundle {
        language_id: "rust".into(),
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

fn provider_metrics(iterations: u64) -> Result<(Value, bool)> {
    let mut reports = serde_json::Map::new();
    let mut all_pass = true;
    for (name, class, text) in [
        (
            "normal",
            DocumentClass::Normal,
            "fn main() { let value = 1; }\n".repeat(256),
        ),
        (
            "large",
            DocumentClass::Large,
            "fn generated() {}\n".repeat(16_384),
        ),
        (
            "pathological",
            DocumentClass::Pathological,
            format!("let {} = 1;\n", "x".repeat(256 * 1_024)),
        ),
    ] {
        let document_id = DocumentId::new(class as u64 + 1);
        let mut actor = ProviderActor::default();
        actor.handle(ProviderRequest::UpdateDocument {
            document_id,
            revision: DocumentRevision::new(1),
            text: text.into_boxed_str(),
            bundle: bundle(),
        })?;
        let mut latency = histogram()?;
        let mut queue = LatestDemandQueue::new(8);
        let mut maximum_depth = 0;
        for sample in 0..iterations.clamp(1, 1_000) {
            for revision in 1..=16 {
                queue.push(
                    document_id,
                    ProviderDemand {
                        revision: DocumentRevision::new(
                            sample.saturating_mul(16).saturating_add(revision),
                        ),
                        visible: std::iter::once(0..4_096).collect(),
                        near_viewport: std::iter::once(4_096..8_192).collect(),
                        priority: Priority::Visible,
                    },
                );
                maximum_depth = maximum_depth.max(queue.depth());
            }
            let queued = queue.pop().context("latest provider demand missing")?;
            let started = Instant::now();
            actor.handle(ProviderRequest::Demand {
                document_id,
                demand: ProviderDemand {
                    revision: DocumentRevision::new(1),
                    ..queued.demand
                },
            })?;
            latency.record(elapsed_nanos(started))?;
        }
        let budget_nanos = class
            .policy()
            .syntax_cpu_budget_micros
            .saturating_mul(1_000);
        let passed = latency.value_at_quantile(0.99) <= budget_nanos && maximum_depth <= 8;
        all_pass &= passed;
        reports.insert(
            name.to_owned(),
            json!({
                "document_class": name,
                "freshness_budget_nanos": budget_nanos,
                "passed": passed,
                "queue_capacity": 8,
                "maximum_queue_depth": maximum_depth,
                "obsolete_demands_dropped": queue.dropped(),
                "distribution": distribution(&latency),
            }),
        );
    }
    Ok((Value::Object(reports), all_pass))
}

fn snapshot_metrics() -> Result<Value> {
    let manager = SnapshotManager::new(SnapshotQuota {
        max_bytes: 64 * 1_024,
        max_revisions: 8,
        held_too_long: Duration::from_secs(5),
    });
    let text: Arc<str> = Arc::from("snapshot text\n".repeat(256));
    let handles = (1..=4)
        .map(|revision| {
            manager.issue(
                "benchmark",
                DocumentId::new(1),
                DocumentRevision::new(revision),
                Arc::clone(&text),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let metrics = manager.metrics()?;
    std::hint::black_box(&handles);
    Ok(json!({
        "live_revisions": metrics.live_revisions,
        "retained_snapshot_bytes": metrics.retained_snapshot_bytes,
        "oldest_live_revision": metrics.oldest_live_revision.map(|revision| revision.get()),
        "held_too_long": metrics.held_too_long.len(),
    }))
}

fn persist_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("wren-save");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    FileSync::sync_parent(path)?;
    Ok(())
}

struct FileSync;

impl FileSync {
    fn sync_parent(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            OpenOptions::new().read(true).open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

fn simulated_remote_metrics(iterations: u64) -> Result<Value> {
    let directory = tempfile::tempdir()?;
    let cache = BlobCache::open(directory.path().join("cache"), 64 * 1_048_576)?;
    let bytes = "remote convergence payload\n".repeat(4_096).into_bytes();
    let hash = *blake3::hash(&bytes).as_bytes();
    let chunks = fastcdc_chunks(&bytes, 4_096, 16_384, 64 * 1_024);
    let mut convergence = histogram()?;
    let mut persisted_save = histogram()?;
    for iteration in 0..iterations.clamp(1, 100) {
        let started = Instant::now();
        let mut materializer = RemoteMaterializer::new(
            DocumentRevision::new(iteration.saturating_add(1)),
            bytes.len() as u64,
            hash,
        );
        for chunk in &chunks {
            materializer.push(&bytes[chunk.range.clone()])?;
        }
        let (_, materialized) = materializer.finish()?;
        cache.put(&materialized)?;
        convergence.record(elapsed_nanos(started))?;

        let started = Instant::now();
        persist_atomically(&directory.path().join("persisted.txt"), &materialized)?;
        persisted_save.record(elapsed_nanos(started))?;
    }
    let verified = cache.get(hash)?.as_deref() == Some(bytes.as_slice());
    Ok(json!({
        "authoritative_gate": false,
        "verified": verified,
        "workspace_generation": WorkspaceGeneration::new(1).get(),
        "bytes": bytes.len(),
        "chunks": chunks.len(),
        "remote_convergence": distribution(&convergence),
        "persisted_save": distribution(&persisted_save),
    }))
}

fn remote_metrics(iterations: u64, spec: &OpenSshSpec) -> Result<(Value, bool)> {
    let nonce = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
    )
    .unwrap_or(u64::MAX)
    .wrapping_add(u64::from(std::process::id()));
    let client_id = ClientId::new(nonce.max(1));
    let document_id = DocumentId::new(nonce.rotate_left(17).max(1));
    let path =
        env::var("WREN_BENCH_REMOTE_PATH").unwrap_or_else(|_| ".wren-system-gates.txt".to_owned());
    let mut client = RemoteWorkspaceClient::connect(spec)?;
    client.heartbeat(nonce)?;
    let opened = client.open(document_id, client_id, path, None)?;
    let mut text = client.blob(opened.content_hash)?;
    let mut revision = opened.revision;
    let mut file_identity = opened.file_identity;
    let mut content_hash = opened.content_hash;
    let mut convergence = histogram()?;
    let mut persisted_save = histogram()?;
    for sample in 1..=iterations.clamp(1, 100) {
        let inserted = format!("remote sample {sample}\n");
        let started = Instant::now();
        let transaction = Transaction::new(
            revision,
            vec![Edit::new(text.len()..text.len(), inserted.clone())],
        )?;
        let mutation = ClientMutation {
            mutation_id: MutationId::new(nonce.wrapping_add(sample).max(1)),
            client_id,
            client_sequence: ClientSequence::new(sample),
            state_deltas: Vec::new(),
            documents: vec![DocumentMutation {
                document_id,
                lease_epoch: opened.lease_epoch,
                base_revision: revision,
                semantic_group_id: SemanticGroupId::new(sample),
                semantic_group_kind: SemanticGroupKind::Operator,
                undo_parent: None,
                transactions: vec![transaction],
            }],
        };
        let result = client.submit(&mutation)?;
        let MutationResult::Durable { documents, .. } = result else {
            anyhow::bail!("remote mutation was not durable: {result:?}");
        };
        revision = documents
            .iter()
            .find(|accepted| accepted.document_id == document_id)
            .context("remote durable result omitted document")?
            .accepted_revision;
        convergence.record(elapsed_nanos(started))?;
        text.extend_from_slice(inserted.as_bytes());

        let started = Instant::now();
        let saved = client.save(&SaveRequest {
            document_id,
            required_frontier: revision,
            expected_file_identity: file_identity,
            expected_content_hash: content_hash,
        })?;
        persisted_save.record(elapsed_nanos(started))?;
        file_identity = saved.new_file_identity;
        content_hash = saved.new_content_hash;
        let materialized = client.blob(content_hash)?;
        anyhow::ensure!(materialized == text, "remote saved blob did not converge");
    }
    client.heartbeat(nonce.wrapping_add(iterations))?;
    client.close()?;
    Ok((
        json!({
            "authoritative_gate": true,
            "passed": true,
            "transport": "dual OpenSSH control/bulk",
            "netem_declared": env::var("WREN_NETEM_ACTIVE").as_deref() == Ok("1"),
            "bytes": text.len(),
            "remote_convergence": distribution(&convergence),
            "persisted_save": distribution(&persisted_save),
        }),
        true,
    ))
}

fn resident_memory_bytes() -> Option<u64> {
    let kib = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmRSS:"))
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
        })
        .or_else(|| {
            let output = Command::new("ps")
                .args(["-o", "rss=", "-p", &std::process::id().to_string()])
                .output()
                .ok()?;
            String::from_utf8(output.stdout)
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
        })?;
    kib.checked_mul(1_024)
}

fn peak_memory_bytes() -> Option<u64> {
    let usage = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF).ok()?;
    let raw = u64::try_from(usage.max_rss()).ok()?;
    #[cfg(target_os = "macos")]
    {
        Some(raw)
    }
    #[cfg(not(target_os = "macos"))]
    {
        raw.checked_mul(1_024)
    }
}

fn main() -> Result<()> {
    let arguments = arguments()?;
    let cpu_pinned = arguments.cpu.is_some_and(pin_cpu);
    let remote_spec = remote_spec_from_environment();
    validate_gate_environment(&arguments, cpu_pinned, remote_spec.is_some())?;
    let memory_before = resident_memory_bytes();
    let (providers, providers_pass) = provider_metrics(arguments.iterations)?;
    let snapshots = snapshot_metrics()?;
    let (remote, remote_pass) = if let Some(spec) = &remote_spec {
        remote_metrics(arguments.iterations, spec)?
    } else {
        (
            json!({
                "available": false,
                "authoritative_gate": false,
                "reason": "remote OpenSSH target not configured",
                "local_algorithm_smoke": simulated_remote_metrics(arguments.iterations)?,
            }),
            false,
        )
    };
    let memory_after = resident_memory_bytes();
    let memory_peak = peak_memory_bytes();
    let passed = providers_pass && remote_pass;
    let report = json!({
        "schema": 1,
        "runner_contract": {
            "cpu_requested": arguments.cpu,
            "cpu_pinned": cpu_pinned,
            "bare_metal_declared": env::var("WREN_BARE_METAL").as_deref() == Ok("1"),
            "netem_declared": env::var("WREN_NETEM_ACTIVE").as_deref() == Ok("1"),
            "gate_authoritative": arguments.gate,
        },
        "provider_freshness_and_queue_depth": providers,
        "snapshot_retention": snapshots,
        "memory": {
            "resident_before_bytes": memory_before,
            "resident_steady_bytes": memory_after,
            "resident_peak_bytes": memory_peak,
            "source": "OS RSS and getrusage",
        },
        "remote_convergence_and_persisted_save": remote,
        "crash_recovery_matrix": {
            "ci_test_suites": ["wren-session", "wren-sessiond"],
            "faults": ["torn-client-wal", "torn-outbox", "torn-session-journal", "checksum-corruption", "crash-after-received", "durable-dedup-retry", "epoch-gap-resume"],
        },
        "passed": passed,
    });
    let rendered = serde_json::to_string_pretty(&report)?;
    if let Some(output) = arguments.output {
        fs::write(&output, format!("{rendered}\n"))
            .with_context(|| format!("write {}", output.display()))?;
    }
    println!("{rendered}");
    if arguments.gate {
        anyhow::ensure!(passed, "system architecture gate failed");
    }
    Ok(())
}
