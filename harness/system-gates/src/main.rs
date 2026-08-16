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

const PROVIDER_NORMAL_P99_GATE_NANOS: u64 = 102_066;
const PROVIDER_LARGE_P99_GATE_NANOS: u64 = 192_613;
const PROVIDER_PATHOLOGICAL_P99_GATE_NANOS: u64 = 225;
const LOOPBACK_REMOTE_CONVERGENCE_BASELINE_NANOS: u64 = 4_886_527;
const LOOPBACK_REMOTE_PERSISTED_SAVE_BASELINE_NANOS: u64 = 19_759_103;

#[derive(Debug)]
struct Arguments {
    iterations: u64,
    cpu: Option<usize>,
    output: Option<PathBuf>,
    remote_baseline_output: Option<PathBuf>,
    gate: bool,
}

fn arguments() -> Result<Arguments> {
    let values = env::args().skip(1).collect::<Vec<_>>();
    let mut iterations = 1_000;
    let mut output = None;
    let mut remote_baseline_output = None;
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
            "--capture-remote-baseline" => {
                index += 1;
                remote_baseline_output = Some(PathBuf::from(
                    values
                        .get(index)
                        .context("--capture-remote-baseline needs a path")?,
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
        remote_baseline_output,
        gate,
    })
}

fn pin_cpu(index: usize) -> bool {
    core_affinity::get_core_ids()
        .and_then(|ids| ids.get(index).copied())
        .is_some_and(core_affinity::set_for_current)
}

fn validate_gate_environment(
    arguments: &Arguments,
    pinned: bool,
    has_remote: bool,
    has_remote_baseline: bool,
) -> Result<()> {
    if !arguments.gate && arguments.remote_baseline_output.is_none() {
        return Ok(());
    }
    anyhow::ensure!(
        env::var("WREN_BARE_METAL").as_deref() == Ok("1"),
        "authoritative gate/baseline capture requires WREN_BARE_METAL=1 on the dedicated benchmark runner"
    );
    anyhow::ensure!(
        arguments.cpu.is_some(),
        "authoritative gate/baseline capture requires --cpu"
    );
    anyhow::ensure!(pinned, "requested benchmark CPU could not be pinned");
    anyhow::ensure!(
        env::var("WREN_NETEM_ACTIVE").as_deref() == Ok("1"),
        "authoritative gate/baseline capture requires WREN_NETEM_ACTIVE=1 with tc netem installed"
    );
    anyhow::ensure!(
        has_remote,
        "authoritative gate/baseline capture requires WREN_BENCH_REMOTE_HOST, WREN_BENCH_REMOTE_WORKSPACE, and WREN_BENCH_REMOTE_STATE"
    );
    if arguments.gate {
        anyhow::ensure!(
            has_remote_baseline,
            "--gate requires WREN_REMOTE_BASELINE_JSON for the active SSH/netem profile"
        );
    }
    Ok(())
}

#[derive(Debug)]
struct RemoteGateProfile {
    profile_id: Box<str>,
    source: Option<PathBuf>,
    convergence_baseline_nanos: u64,
    persisted_save_baseline_nanos: u64,
    convergence_gate_nanos: u64,
    persisted_save_gate_nanos: u64,
}

const fn ten_percent_cut(baseline_nanos: u64) -> u64 {
    baseline_nanos.saturating_mul(9) / 10
}

fn remote_gate_profile_from_path(path: Option<PathBuf>) -> Result<RemoteGateProfile> {
    let Some(path) = path else {
        return Ok(RemoteGateProfile {
            profile_id: "local-dual-openssh-loopback".into(),
            source: None,
            convergence_baseline_nanos: LOOPBACK_REMOTE_CONVERGENCE_BASELINE_NANOS,
            persisted_save_baseline_nanos: LOOPBACK_REMOTE_PERSISTED_SAVE_BASELINE_NANOS,
            convergence_gate_nanos: ten_percent_cut(LOOPBACK_REMOTE_CONVERGENCE_BASELINE_NANOS),
            persisted_save_gate_nanos: ten_percent_cut(
                LOOPBACK_REMOTE_PERSISTED_SAVE_BASELINE_NANOS,
            ),
        });
    };
    let source = fs::read_to_string(&path)
        .with_context(|| format!("read remote baseline {}", path.display()))?;
    let report: Value = serde_json::from_str(&source)
        .with_context(|| format!("parse remote baseline {}", path.display()))?;
    let schema = report.get("schema").and_then(Value::as_u64);
    let profile_id = report
        .get("profile_id")
        .and_then(Value::as_str)
        .filter(|profile_id| !profile_id.is_empty());
    let convergence_baseline_nanos = report
        .get("convergence_baseline_p99_nanos")
        .and_then(Value::as_u64)
        .filter(|baseline| *baseline > 0);
    let persisted_save_baseline_nanos = report
        .get("persisted_save_baseline_p99_nanos")
        .and_then(Value::as_u64)
        .filter(|baseline| *baseline > 0);
    anyhow::ensure!(
        schema == Some(1)
            && profile_id.is_some()
            && convergence_baseline_nanos.is_some()
            && persisted_save_baseline_nanos.is_some(),
        "remote baseline must be schema 1 with profile_id, convergence_baseline_p99_nanos, and persisted_save_baseline_p99_nanos"
    );
    let convergence_baseline_nanos = convergence_baseline_nanos.unwrap_or_default();
    let persisted_save_baseline_nanos = persisted_save_baseline_nanos.unwrap_or_default();
    Ok(RemoteGateProfile {
        profile_id: profile_id.unwrap_or_default().into(),
        source: Some(path),
        convergence_baseline_nanos,
        persisted_save_baseline_nanos,
        convergence_gate_nanos: ten_percent_cut(convergence_baseline_nanos),
        persisted_save_gate_nanos: ten_percent_cut(persisted_save_baseline_nanos),
    })
}

fn remote_spec_from_environment() -> Option<OpenSshSpec> {
    let host = env::var("WREN_BENCH_REMOTE_HOST").ok()?;
    let workspace = env::var_os("WREN_BENCH_REMOTE_WORKSPACE").map(PathBuf::from)?;
    let state = env::var_os("WREN_BENCH_REMOTE_STATE").map(PathBuf::from)?;
    let mut extra_options = vec!["BatchMode=yes".into()];
    extra_options.extend(
        env::var("WREN_BENCH_SSH_OPTIONS")
            .ok()
            .into_iter()
            .flat_map(|options| {
                options
                    .split(',')
                    .map(str::trim)
                    .filter(|option| !option.is_empty())
                    .map(Box::<str>::from)
                    .collect::<Vec<_>>()
            }),
    );
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
        extra_options,
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

const fn provider_p99_gate_nanos(class: DocumentClass) -> u64 {
    match class {
        DocumentClass::Normal => PROVIDER_NORMAL_P99_GATE_NANOS,
        DocumentClass::Large => PROVIDER_LARGE_P99_GATE_NANOS,
        DocumentClass::Pathological => PROVIDER_PATHOLOGICAL_P99_GATE_NANOS,
    }
}

fn provider_metrics(iterations: u64) -> Result<(Value, bool)> {
    // The remote portion caps costly SSH round trips at 100, but a p99 from
    // 100 samples degenerates into the single maximum. Keep provider tails
    // statistically meaningful even when the combined harness uses that cap.
    let provider_iterations = iterations.clamp(1_000, 10_000);
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
        for sample in 0..provider_iterations {
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
            std::hint::black_box(actor.handle(ProviderRequest::Demand {
                document_id,
                demand: ProviderDemand {
                    revision: DocumentRevision::new(1),
                    ..queued.demand
                },
            })?);
            latency.record(elapsed_nanos(started))?;
        }
        let gate_nanos = provider_p99_gate_nanos(class);
        let passed = latency.value_at_quantile(0.99) < gate_nanos && maximum_depth <= 8;
        all_pass &= passed;
        reports.insert(
            name.to_owned(),
            json!({
                "document_class": name,
                "hard_gate": true,
                "p99_gate_nanos": gate_nanos,
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

fn remote_metrics(
    iterations: u64,
    spec: &OpenSshSpec,
    gates: &RemoteGateProfile,
) -> Result<(Value, bool)> {
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
    let convergence_pass = convergence.value_at_quantile(0.99) < gates.convergence_gate_nanos;
    let persisted_save_pass =
        persisted_save.value_at_quantile(0.99) < gates.persisted_save_gate_nanos;
    let passed = convergence_pass && persisted_save_pass;
    Ok((
        json!({
            "hard_gate": true,
            "authoritative_gate": env::var("WREN_BARE_METAL").as_deref() == Ok("1")
                && env::var("WREN_NETEM_ACTIVE").as_deref() == Ok("1")
                && gates.source.is_some(),
            "passed": passed,
            "transport": "dual OpenSSH control/bulk",
            "netem_declared": env::var("WREN_NETEM_ACTIVE").as_deref() == Ok("1"),
            "gate_profile": {
                "profile_id": gates.profile_id,
                "source": gates.source,
            },
            "bytes": text.len(),
            "remote_convergence": {
                "hard_gate": true,
                "baseline_p99_nanos": gates.convergence_baseline_nanos,
                "p99_gate_nanos": gates.convergence_gate_nanos,
                "passed": convergence_pass,
                "distribution": distribution(&convergence),
            },
            "persisted_save": {
                "hard_gate": true,
                "baseline_p99_nanos": gates.persisted_save_baseline_nanos,
                "p99_gate_nanos": gates.persisted_save_gate_nanos,
                "passed": persisted_save_pass,
                "distribution": distribution(&persisted_save),
            },
        }),
        passed,
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
    let remote_gate_profile =
        remote_gate_profile_from_path(env::var_os("WREN_REMOTE_BASELINE_JSON").map(PathBuf::from))?;
    validate_gate_environment(
        &arguments,
        cpu_pinned,
        remote_spec.is_some(),
        remote_gate_profile.source.is_some(),
    )?;
    let memory_before = resident_memory_bytes();
    let (providers, providers_pass) = provider_metrics(arguments.iterations)?;
    let snapshots = snapshot_metrics()?;
    let (remote, remote_pass) = if let Some(spec) = &remote_spec {
        remote_metrics(arguments.iterations, spec, &remote_gate_profile)?
    } else {
        (
            json!({
                "available": false,
                "hard_gate": true,
                "passed": false,
                "authoritative_gate": false,
                "reason": "hard gate requires a real dual-OpenSSH target",
                "local_algorithm_smoke": simulated_remote_metrics(arguments.iterations)?,
            }),
            false,
        )
    };
    let memory_after = resident_memory_bytes();
    let memory_peak = peak_memory_bytes();
    let passed = providers_pass && remote_pass;
    if let Some(path) = &arguments.remote_baseline_output {
        let convergence_p99_nanos = remote
            .pointer("/remote_convergence/distribution/p99")
            .and_then(Value::as_u64)
            .context("captured remote convergence p99 is missing")?;
        let persisted_save_p99_nanos = remote
            .pointer("/persisted_save/distribution/p99")
            .and_then(Value::as_u64)
            .context("captured persisted-save p99 is missing")?;
        let profile_id = env::var("WREN_REMOTE_PROFILE_ID")
            .context("baseline capture requires WREN_REMOTE_PROFILE_ID")?;
        anyhow::ensure!(
            !profile_id.is_empty(),
            "WREN_REMOTE_PROFILE_ID cannot be empty"
        );
        let baseline = json!({
            "schema": 1,
            "profile_id": profile_id,
            "convergence_baseline_p99_nanos": convergence_p99_nanos,
            "persisted_save_baseline_p99_nanos": persisted_save_p99_nanos,
        });
        fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&baseline)?),
        )
        .with_context(|| format!("write remote baseline {}", path.display()))?;
    }
    let report = json!({
        "schema": 3,
        "runner_contract": {
            "cpu_requested": arguments.cpu,
            "cpu_pinned": cpu_pinned,
            "bare_metal_declared": env::var("WREN_BARE_METAL").as_deref() == Ok("1"),
            "netem_declared": env::var("WREN_NETEM_ACTIVE").as_deref() == Ok("1"),
            "gate_authoritative": arguments.gate,
            "baseline_capture_authoritative": arguments.remote_baseline_output.is_some(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_profile_uses_a_strict_ten_percent_cut() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("remote-baseline.json");
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "schema": 1,
                "profile_id": "netem-test",
                "convergence_baseline_p99_nanos": 1_001,
                "persisted_save_baseline_p99_nanos": 2_001,
            }))
            .expect("serialize baseline"),
        )
        .expect("write baseline");

        let profile = remote_gate_profile_from_path(Some(path)).expect("load baseline");
        assert_eq!(profile.convergence_gate_nanos, 900);
        assert_eq!(profile.persisted_save_gate_nanos, 1_800);
        assert_eq!(profile.profile_id.as_ref(), "netem-test");
        assert!(profile.source.is_some());
    }

    #[test]
    fn loopback_profile_is_hard_but_not_authoritative() {
        let profile = remote_gate_profile_from_path(None).expect("loopback profile");
        assert_eq!(profile.convergence_gate_nanos, 4_397_874);
        assert_eq!(profile.persisted_save_gate_nanos, 17_783_192);
        assert!(profile.source.is_none());
    }
}
