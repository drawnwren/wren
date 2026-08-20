#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use wasmtime::component::{Component, Linker, TypedFunc};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wren_types::{DocumentId, DocumentRevision};

#[cfg(test)]
const SPIKE_COMPONENT: &str = include_str!("spike.wat");

pub const EXTENSION_API_VERSION: &str = "1.0.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostPlacement {
    Client,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderCapability {
    CompletionSource,
    DecorationProvider,
    PickerProvider,
    TaskProvider,
    StructuralProvider,
    CommandHandler,
    StatusItemProvider,
    VirtualDocumentProvider,
    UiContributions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityGrant {
    DocumentReadSnapshot,
    DocumentProposeEdit,
    WorkspaceRead,
    WorkspaceSearch,
    TaskSpawn,
    ClientNotify,
    ClientOpenUrl,
    RawFilesystem,
    RawSockets,
}

impl CapabilityGrant {
    #[must_use]
    pub const fn high_trust(self) -> bool {
        matches!(self, Self::RawFilesystem | Self::RawSockets)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestCapability {
    pub kind: ProviderCapability,
    pub placement: HostPlacement,
    #[serde(default)]
    pub grants: BTreeSet<CapabilityGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestCommand {
    pub name: Box<str>,
    pub description: Box<str>,
    #[serde(default)]
    pub arguments: BTreeMap<Box<str>, Box<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionManifest {
    pub id: Box<str>,
    pub name: Box<str>,
    pub version: Box<str>,
    pub api_version: Box<str>,
    pub component: Box<str>,
    #[serde(default)]
    pub capabilities: Vec<ManifestCapability>,
    #[serde(default)]
    pub commands: Vec<ManifestCommand>,
    #[serde(default)]
    pub default_keybindings: BTreeMap<Box<str>, Box<str>>,
    #[serde(default)]
    pub settings_schema: BTreeMap<Box<str>, Box<str>>,
    #[serde(default)]
    pub languages: Vec<Box<str>>,
    #[serde(default)]
    pub queries: Vec<Box<str>>,
    #[serde(default)]
    pub snippets: Vec<Box<str>>,
    #[serde(default)]
    pub themes: Vec<Box<str>>,
}

impl ExtensionManifest {
    pub fn parse(source: &str) -> Result<Self, ExtensionError> {
        let manifest: Self = toml::from_str(source).map_err(|error| ExtensionError::Manifest(error.to_string().into()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), ExtensionError> {
        if self.api_version.as_ref() != EXTENSION_API_VERSION {
            return Err(ExtensionError::ApiVersion { supported: EXTENSION_API_VERSION.into(), requested: self.api_version.clone() });
        }
        if self.id.is_empty() || !self.id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')) {
            return Err(ExtensionError::Manifest("invalid extension ID".into()));
        }
        if self.component.is_empty() {
            return Err(ExtensionError::Manifest("component path must not be empty".into()));
        }
        for capability in &self.capabilities {
            if capability.kind == ProviderCapability::TaskProvider && capability.placement != HostPlacement::Workspace {
                return Err(ExtensionError::Placement("task-provider must run in the workspace host".into()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DeclarativeUi {
    TextDocument { title: Box<str>, text: Box<str> },
    VirtualList { title: Box<str>, items: Vec<Box<str>> },
    Tree { title: Box<str>, roots: Vec<Box<str>> },
    Table { title: Box<str>, columns: Vec<Box<str>>, rows: Vec<Vec<Box<str>>> },
    Form { title: Box<str>, fields: Vec<Box<str>> },
    DiffView { title: Box<str>, before: Box<str>, after: Box<str> },
    Panel { title: Box<str>, children: Vec<DeclarativeUi> },
    Picker { title: Box<str>, items: Vec<Box<str>> },
    Notification { message: Box<str> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityPolicy {
    granted: BTreeSet<CapabilityGrant>,
    high_trust: bool,
}

impl CapabilityPolicy {
    #[must_use]
    pub fn new(granted: BTreeSet<CapabilityGrant>, high_trust: bool) -> Self {
        Self { granted, high_trust }
    }

    pub fn authorize(&self, grant: CapabilityGrant) -> Result<(), ExtensionError> {
        if !self.granted.contains(&grant) || (grant.high_trust() && !self.high_trust) {
            return Err(ExtensionError::CapabilityDenied(grant));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub document_id: DocumentId,
    pub document_revision: DocumentRevision,
    pub prefix: Box<str>,
    pub max_candidates: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionCandidate {
    pub label: Box<str>,
    pub insert_text: Box<str>,
    pub kind: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecorationRequest {
    pub document_id: DocumentId,
    pub document_revision: DocumentRevision,
    pub visible: Vec<std::ops::Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decoration {
    pub range: std::ops::Range<usize>,
    pub class: Box<str>,
}

pub trait CompletionSource {
    fn complete(&self, request: CompletionRequest) -> Result<CompletionStream, ExtensionError>;
}

pub trait DecorationProvider {
    fn decorate(&self, request: &DecorationRequest) -> Result<Vec<Decoration>, ExtensionError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionResourcePolicy {
    pub max_memory_bytes: usize,
    pub max_table_elements: usize,
    pub max_instances: usize,
    pub fuel_per_request: u64,
    pub max_concurrent_requests: usize,
    pub max_result_bytes: usize,
    pub chunk_candidates: usize,
    pub request_deadline: Duration,
}

impl Default for ExtensionResourcePolicy {
    fn default() -> Self {
        Self {
            max_memory_bytes: 16 * 1_048_576,
            max_table_elements: 1_024,
            max_instances: 4,
            fuel_per_request: 1_000_000,
            max_concurrent_requests: 4,
            max_result_bytes: 1_048_576,
            chunk_candidates: 64,
            request_deadline: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionHostStats {
    pub restarts: u64,
    pub traps: u64,
    pub cancelled_streams: u64,
    pub backpressure_events: u64,
    pub active_requests: usize,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExtensionError {
    #[error("extension runtime failed: {0}")]
    Runtime(Box<str>),
    #[error("extension result exceeded its {limit}-byte quota")]
    ResultQuota { limit: usize },
    #[error("extension request was cancelled")]
    Cancelled,
    #[error("extension host is at its {limit}-request concurrency limit")]
    ConcurrencyLimit { limit: usize },
    #[error("spawn extension producer: {0}")]
    Spawn(Box<str>),
    #[error("extension manifest is invalid: {0}")]
    Manifest(Box<str>),
    #[error("extension requests API {requested}, but this host supports {supported}")]
    ApiVersion { supported: Box<str>, requested: Box<str> },
    #[error("extension capability placement is invalid: {0}")]
    Placement(Box<str>),
    #[error("extension capability {0:?} was denied")]
    CapabilityDenied(CapabilityGrant),
}

struct StoreData {
    limits: StoreLimits,
}

struct Runtime {
    store: Store<StoreData>,
    completion_count: TypedFunc<(u32,), (u32,)>,
    decoration_count: TypedFunc<(u32,), (u32,)>,
    #[cfg(test)]
    burn: TypedFunc<(), ()>,
}

struct HostInner {
    #[cfg(test)]
    placement: HostPlacement,
    engine: Engine,
    component: Component,
    policy: ExtensionResourcePolicy,
    runtime: Mutex<Runtime>,
    active_requests: AtomicUsize,
    restarts: AtomicU64,
    traps: AtomicU64,
    cancelled_streams: AtomicU64,
    backpressure_events: AtomicU64,
}

#[derive(Clone)]
pub struct WasmExtensionHost {
    inner: Arc<HostInner>,
}

impl WasmExtensionHost {
    #[cfg(test)]
    pub fn new(placement: HostPlacement, policy: ExtensionResourcePolicy) -> Result<Self, ExtensionError> {
        Self::from_component_bytes(placement, policy, SPIKE_COMPONENT.as_bytes())
    }

    /// Instantiates an installed component in its own store. The current v1
    /// runtime adapter requires the WIT-shaped completion/decorations exports;
    /// capability imports remain host-mediated by the manifest policy.
    pub fn from_component_bytes(placement: HostPlacement, policy: ExtensionResourcePolicy, component_bytes: &[u8]) -> Result<Self, ExtensionError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(runtime_error)?;
        let component = Component::new(&engine, component_bytes).map_err(runtime_error)?;
        Self::from_component(placement, policy, engine, component)
    }

    fn from_component(_placement: HostPlacement, policy: ExtensionResourcePolicy, engine: Engine, component: Component) -> Result<Self, ExtensionError> {
        let runtime = instantiate(&engine, &component, policy)?;
        Ok(Self {
            inner: Arc::new(HostInner {
                #[cfg(test)]
                placement: _placement,
                engine,
                component,
                policy,
                runtime: Mutex::new(runtime),
                active_requests: AtomicUsize::new(0),
                restarts: AtomicU64::new(0),
                traps: AtomicU64::new(0),
                cancelled_streams: AtomicU64::new(0),
                backpressure_events: AtomicU64::new(0),
            }),
        })
    }

    #[cfg(test)]
    fn reconfigured(&self, policy: ExtensionResourcePolicy) -> Result<Self, ExtensionError> {
        Self::from_component(self.inner.placement, policy, self.inner.engine.clone(), self.inner.component.clone())
    }

    #[must_use]
    #[cfg(test)]
    pub fn placement(&self) -> HostPlacement {
        self.inner.placement
    }

    #[must_use]
    pub fn stats(&self) -> ExtensionHostStats {
        ExtensionHostStats {
            restarts: self.inner.restarts.load(Ordering::Acquire),
            traps: self.inner.traps.load(Ordering::Acquire),
            cancelled_streams: self.inner.cancelled_streams.load(Ordering::Acquire),
            backpressure_events: self.inner.backpressure_events.load(Ordering::Acquire),
            active_requests: self.inner.active_requests.load(Ordering::Acquire),
        }
    }

    #[must_use]
    pub fn request_deadline(&self) -> Duration {
        self.inner.policy.request_deadline
    }

    #[cfg(test)]
    pub fn exercise_cpu_quota_and_restart(&self) -> Result<(), ExtensionError> {
        let call = self.with_runtime(|runtime| {
            let function = runtime.burn;
            function.call(&mut runtime.store, ()).map_err(runtime_error)
        });
        match call {
            Ok(()) => Err(ExtensionError::Runtime("non-terminating component unexpectedly completed".into())),
            Err(_) => Ok(()),
        }
    }

    fn restart(&self) -> Result<(), ExtensionError> {
        let replacement = instantiate(&self.inner.engine, &self.inner.component, self.inner.policy)?;
        *self.inner.runtime.lock() = replacement;
        self.inner.restarts.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn acquire_request(&self) -> Result<RequestPermit, ExtensionError> {
        let limit = self.inner.policy.max_concurrent_requests;
        self.inner
            .active_requests
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| (active < limit).then_some(active + 1))
            .map_err(|_| ExtensionError::ConcurrencyLimit { limit })?;
        Ok(RequestPermit { inner: Arc::clone(&self.inner) })
    }

    fn component_completion_count(&self, prefix_bytes: usize) -> Result<usize, ExtensionError> {
        let prefix_bytes = u32::try_from(prefix_bytes).unwrap_or(u32::MAX);
        self.with_runtime(|runtime| {
            let function = runtime.completion_count;
            let (count,) = function.call(&mut runtime.store, (prefix_bytes,)).map_err(runtime_error)?;
            Ok(count as usize)
        })
    }

    fn with_runtime<T>(&self, call: impl FnOnce(&mut Runtime) -> Result<T, ExtensionError>) -> Result<T, ExtensionError> {
        let result = {
            let mut runtime = self.inner.runtime.lock();
            runtime.store.set_fuel(self.inner.policy.fuel_per_request).map_err(runtime_error).and_then(|()| call(&mut runtime))
        };
        if result.is_err() {
            self.inner.traps.fetch_add(1, Ordering::AcqRel);
            self.restart()?;
        }
        result
    }
}

impl CompletionSource for WasmExtensionHost {
    fn complete(&self, request: CompletionRequest) -> Result<CompletionStream, ExtensionError> {
        let permit = self.acquire_request()?;
        let total = self.component_completion_count(request.prefix.len())?.min(request.max_candidates as usize);
        let cancelled = Arc::new(AtomicBool::new(false));
        let producer_cancelled = Arc::clone(&cancelled);
        let (sender, receiver) = mpsc::sync_channel(1);
        let inner = Arc::clone(&self.inner);
        let join = thread::Builder::new()
            .name("wren-extension-stream".into())
            .spawn(move || {
                produce_completions(&inner, &request, total, &producer_cancelled, &sender);
                drop(permit);
            })
            .map_err(|error| ExtensionError::Spawn(error.to_string().into()))?;
        Ok(CompletionStream { receiver, cancelled, join: Some(join) })
    }
}

impl DecorationProvider for WasmExtensionHost {
    fn decorate(&self, request: &DecorationRequest) -> Result<Vec<Decoration>, ExtensionError> {
        let _permit = self.acquire_request()?;
        let visible_count = u32::try_from(request.visible.len()).unwrap_or(u32::MAX);
        let count = self.with_runtime(|runtime| {
            let function = runtime.decoration_count;
            function.call(&mut runtime.store, (visible_count,)).map(|(count,)| count).map_err(runtime_error)
        })?;
        let decorations = request
            .visible
            .iter()
            .cycle()
            .take(count as usize)
            .cloned()
            .map(|range| Decoration { range, class: "spike.decoration".into() })
            .collect::<Vec<_>>();
        let bytes = decorations.iter().fold(0_usize, |total, decoration| total.saturating_add(decoration.class.len() + 16));
        if bytes > self.inner.policy.max_result_bytes {
            return Err(ExtensionError::ResultQuota { limit: self.inner.policy.max_result_bytes });
        }
        Ok(decorations)
    }
}

pub struct CompletionStream {
    receiver: Receiver<Result<Vec<CompletionCandidate>, ExtensionError>>,
    cancelled: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl CompletionStream {
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<Vec<CompletionCandidate>>, ExtensionError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(Ok(chunk)) => Ok(Some(chunk)),
            Ok(Err(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(None),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ExtensionError::Runtime("timed out waiting for extension stream".into())),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl Drop for CompletionStream {
    fn drop(&mut self) {
        self.cancel();
        while self.receiver.try_recv().is_ok() {}
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct RequestPermit {
    inner: Arc<HostInner>,
}

impl Drop for RequestPermit {
    fn drop(&mut self) {
        self.inner.active_requests.fetch_sub(1, Ordering::AcqRel);
    }
}

fn produce_completions(
    inner: &HostInner,
    request: &CompletionRequest,
    total: usize,
    cancelled: &AtomicBool,
    sender: &SyncSender<Result<Vec<CompletionCandidate>, ExtensionError>>,
) {
    let chunk_size = inner.policy.chunk_candidates.max(1);
    let mut produced = 0;
    let mut result_bytes = 0_usize;
    while produced < total {
        if cancelled.load(Ordering::Acquire) {
            inner.cancelled_streams.fetch_add(1, Ordering::AcqRel);
            let _ = try_send_cancel(sender, cancelled, Err(ExtensionError::Cancelled), inner);
            return;
        }
        let end = produced.saturating_add(chunk_size).min(total);
        let chunk = (produced..end)
            .map(|index| {
                let label = format!("{}-{index}", request.prefix);
                CompletionCandidate { insert_text: label.clone().into(), label: label.into(), kind: "spike".into() }
            })
            .collect::<Vec<_>>();
        result_bytes = chunk.iter().fold(result_bytes, |bytes, candidate| {
            bytes.saturating_add(candidate.label.len()).saturating_add(candidate.insert_text.len()).saturating_add(candidate.kind.len())
        });
        if result_bytes > inner.policy.max_result_bytes {
            let _ = try_send_cancel(sender, cancelled, Err(ExtensionError::ResultQuota { limit: inner.policy.max_result_bytes }), inner);
            return;
        }
        if !try_send_cancel(sender, cancelled, Ok(chunk), inner) {
            inner.cancelled_streams.fetch_add(1, Ordering::AcqRel);
            return;
        }
        produced = end;
    }
}

fn try_send_cancel(
    sender: &SyncSender<Result<Vec<CompletionCandidate>, ExtensionError>>,
    cancelled: &AtomicBool,
    mut value: Result<Vec<CompletionCandidate>, ExtensionError>,
    inner: &HostInner,
) -> bool {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return false;
        }
        match sender.try_send(value) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                value = returned;
                inner.backpressure_events.fetch_add(1, Ordering::AcqRel);
                thread::yield_now();
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

fn instantiate(engine: &Engine, component: &Component, policy: ExtensionResourcePolicy) -> Result<Runtime, ExtensionError> {
    let limits =
        StoreLimitsBuilder::new().memory_size(policy.max_memory_bytes).table_elements(policy.max_table_elements).instances(policy.max_instances).build();
    let mut store = Store::new(engine, StoreData { limits });
    store.limiter(|data| &mut data.limits);
    store.set_fuel(policy.fuel_per_request).map_err(runtime_error)?;
    let instance = Linker::new(engine).instantiate(&mut store, component).map_err(runtime_error)?;
    let completion_count = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "completion-count").map_err(runtime_error)?;
    let decoration_count = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "decoration-count").map_err(runtime_error)?;
    #[cfg(test)]
    let burn = instance.get_typed_func::<(), ()>(&mut store, "burn").map_err(runtime_error)?;
    Ok(Runtime {
        store,
        completion_count,
        decoration_count,
        #[cfg(test)]
        burn,
    })
}

fn runtime_error(error: impl std::fmt::Display) -> ExtensionError {
    ExtensionError::Runtime(error.to_string().into())
}

pub struct ExtensionRegistry {
    placement: HostPlacement,
    high_trust: bool,
    extensions: BTreeMap<Box<str>, (ExtensionManifest, WasmExtensionHost)>,
}

impl ExtensionRegistry {
    #[must_use]
    pub fn new(placement: HostPlacement, high_trust: bool) -> Self {
        Self { placement, high_trust, extensions: BTreeMap::new() }
    }

    #[cfg(test)]
    pub fn install(&mut self, manifest: ExtensionManifest, policy: ExtensionResourcePolicy) -> Result<(), ExtensionError> {
        self.install_component(manifest, policy, SPIKE_COMPONENT.as_bytes())
    }

    pub fn install_component(&mut self, manifest: ExtensionManifest, policy: ExtensionResourcePolicy, component_bytes: &[u8]) -> Result<(), ExtensionError> {
        manifest.validate()?;
        let grants = manifest
            .capabilities
            .iter()
            .filter(|capability| capability.placement == self.placement)
            .flat_map(|capability| capability.grants.iter().copied())
            .collect::<BTreeSet<_>>();
        let capability_policy = CapabilityPolicy::new(grants.clone(), self.high_trust);
        for grant in grants {
            capability_policy.authorize(grant)?;
        }
        let host = WasmExtensionHost::from_component_bytes(self.placement, policy, component_bytes)?;
        self.extensions.insert(manifest.id.clone(), (manifest, host));
        Ok(())
    }

    pub fn uninstall(&mut self, id: &str) -> bool {
        self.extensions.remove(id).is_some()
    }

    #[cfg(test)]
    pub fn manifest(&self, id: &str) -> Option<&ExtensionManifest> {
        self.extensions.get(id).map(|(manifest, _)| manifest)
    }

    pub fn host(&self, id: &str) -> Option<&WasmExtensionHost> {
        self.extensions.get(id).map(|(_, host)| host)
    }

    #[must_use]
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.extensions.len()
    }

    #[must_use]
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.extensions.is_empty()
    }

    /// A host-process crash drops every store. Recreate each independently so
    /// one bad component cannot prevent healthy manifests from returning.
    #[cfg(test)]
    pub fn restart_all(&mut self, policy: ExtensionResourcePolicy) -> Vec<(Box<str>, ExtensionError)> {
        let mut failures = Vec::new();
        for (id, (_, host)) in &mut self.extensions {
            match host.reconfigured(policy) {
                Ok(replacement) => *host = replacement,
                Err(error) => failures.push((id.clone(), error)),
            }
        }
        failures
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum HostRequest {
    Hello,
    Install { manifest: Box<ExtensionManifest>, component: Vec<u8> },
    Uninstall { id: Box<str> },
    CompleteExtension { id: Box<str>, request: CompletionRequest },
    DecorateExtension { id: Box<str>, request: DecorationRequest },
    ExtensionStats { id: Box<str> },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum HostResponse {
    Hello { api_version: Box<str>, placement: HostPlacement },
    Stats { stats: ExtensionHostStats },
    Completions { candidates: Vec<CompletionCandidate> },
    Decorations { decorations: Vec<Decoration> },
    Ok,
    Installed { id: Box<str> },
    Uninstalled { id: Box<str>, existed: bool },
    Error { message: Box<str> },
}

pub fn run_stdio_host(placement: HostPlacement) -> Result<(), ExtensionError> {
    let mut registry = ExtensionRegistry::new(placement, false);
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| ExtensionError::Runtime(error.to_string().into()))?;
        let request = serde_json::from_str::<HostRequest>(&line).map_err(|error| ExtensionError::Runtime(error.to_string().into()))?;
        let shutdown = request == HostRequest::Shutdown;
        let response = handle_host_request(placement, &mut registry, request);
        serde_json::to_writer(&mut stdout, &response).map_err(|error| ExtensionError::Runtime(error.to_string().into()))?;
        stdout.write_all(b"\n").and_then(|()| stdout.flush()).map_err(|error| ExtensionError::Runtime(error.to_string().into()))?;
        if shutdown {
            break;
        }
    }
    Ok(())
}

fn handle_host_request(placement: HostPlacement, registry: &mut ExtensionRegistry, request: HostRequest) -> HostResponse {
    let result = match request {
        HostRequest::Hello => Ok(HostResponse::Hello { api_version: EXTENSION_API_VERSION.into(), placement }),
        HostRequest::Install { manifest, component } => {
            let id = manifest.id.clone();
            registry.install_component(*manifest, ExtensionResourcePolicy::default(), &component).map(|()| HostResponse::Installed { id })
        }
        HostRequest::Uninstall { id } => Ok(HostResponse::Uninstalled { existed: registry.uninstall(&id), id }),
        HostRequest::CompleteExtension { id, request } => registry
            .host(&id)
            .ok_or_else(|| ExtensionError::Manifest(format!("unknown extension {id}").into()))
            .and_then(|host| collect_completions(host, request))
            .map(|candidates| HostResponse::Completions { candidates }),
        HostRequest::DecorateExtension { id, request } => registry
            .host(&id)
            .ok_or_else(|| ExtensionError::Manifest(format!("unknown extension {id}").into()))
            .and_then(|host| host.decorate(&request))
            .map(|decorations| HostResponse::Decorations { decorations }),
        HostRequest::ExtensionStats { id } => registry
            .host(&id)
            .ok_or_else(|| ExtensionError::Manifest(format!("unknown extension {id}").into()))
            .map(|host| HostResponse::Stats { stats: host.stats() }),
        HostRequest::Shutdown => Ok(HostResponse::Ok),
    };
    result.unwrap_or_else(|error| HostResponse::Error { message: error.to_string().into() })
}

fn collect_completions(host: &WasmExtensionHost, request: CompletionRequest) -> Result<Vec<CompletionCandidate>, ExtensionError> {
    let stream = host.complete(request)?;
    let mut candidates = Vec::new();
    while let Some(mut chunk) = stream.recv_timeout(host.request_deadline())? {
        candidates.append(&mut chunk);
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn host(policy: ExtensionResourcePolicy) -> WasmExtensionHost {
        WasmExtensionHost::new(HostPlacement::Client, policy).expect("component host")
    }

    fn completion(max_candidates: u32) -> CompletionRequest {
        CompletionRequest { document_id: DocumentId::new(1), document_revision: DocumentRevision::new(2), prefix: "wr".into(), max_candidates }
    }

    #[test]
    fn wit_prototypes_parse_including_async_streams() {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("wit");
        let mut resolve = wit_parser::Resolve::default();
        let (package, files) = resolve.push_dir(&directory).expect("parse WIT package");
        assert!(files.paths().next().is_some());
        assert_eq!(resolve.packages[package].name.namespace, "wren");
        assert_eq!(resolve.packages[package].name.version.as_ref().map(ToString::to_string).as_deref(), Some(EXTENSION_API_VERSION));
    }

    #[test]
    fn v1_manifest_contributions_are_typed_and_capabilities_are_fenced() {
        let manifest = ExtensionManifest::parse(
            r#"
id = "example.tools"
name = "Example Tools"
version = "2.3.4"
api_version = "1.0.0"
component = "example-tools.wasm"
languages = ["example-language.toml"]
queries = ["queries/highlights.scm"]
snippets = ["snippets/example.toml"]
themes = ["themes/example.toml"]

[default_keybindings]
"space x" = "example.run"

[settings_schema]
level = "integer"

[[commands]]
name = "example.run"
description = "Run the example"

[[capabilities]]
kind = "completion-source"
placement = "client"
grants = ["document-read-snapshot"]
"#,
        )
        .expect("manifest");
        assert_eq!(manifest.commands[0].name.as_ref(), "example.run");
        assert_eq!(manifest.capabilities[0].placement, HostPlacement::Client);

        let raw = CapabilityPolicy::new([CapabilityGrant::RawFilesystem].into_iter().collect(), false);
        assert_eq!(raw.authorize(CapabilityGrant::RawFilesystem), Err(ExtensionError::CapabilityDenied(CapabilityGrant::RawFilesystem)));
    }

    #[test]
    fn declarative_ui_serializes_semantics_without_terminal_coordinates() {
        let model = DeclarativeUi::Panel {
            title: "Review".into(),
            children: vec![DeclarativeUi::DiffView { title: "proposal".into(), before: "old".into(), after: "new".into() }],
        };
        let json = serde_json::to_string(&model).expect("serialize UI");
        assert!(json.contains("diff-view"));
        assert!(!json.contains("window-id"));
        assert!(!json.contains("coordinates"));
    }

    #[test]
    fn registry_lifecycle_instantiates_and_removes_extensions() {
        let manifest = ExtensionManifest::parse(
            r#"
id = "example.lifecycle"
name = "Lifecycle"
version = "1.0.0"
api_version = "1.0.0"
component = "lifecycle.wasm"

[[capabilities]]
kind = "decoration-provider"
placement = "client"
grants = ["document-read-snapshot"]
"#,
        )
        .expect("manifest");
        let mut registry = ExtensionRegistry::new(HostPlacement::Client, false);
        let custom_component = SPIKE_COMPONENT.replace("i32.const 3", "i32.const 9");
        registry.install_component(manifest, ExtensionResourcePolicy::default(), custom_component.as_bytes()).expect("install");
        let before = collect_completions(registry.host("example.lifecycle").expect("host"), completion(20)).expect("custom component before restart");
        assert_eq!(before.len(), 11);
        assert!(registry.restart_all(ExtensionResourcePolicy::default()).is_empty());
        let after = collect_completions(registry.host("example.lifecycle").expect("restarted host"), completion(20)).expect("custom component after restart");
        assert_eq!(after.len(), 11);
        assert!(registry.uninstall("example.lifecycle"));
        assert!(registry.is_empty());
    }

    #[test]
    fn completion_component_streams_bounded_chunks_and_cancels_under_backpressure() {
        let policy = ExtensionResourcePolicy { chunk_candidates: 1, ..ExtensionResourcePolicy::default() };
        let host = host(policy);
        let stream = host.complete(completion(5)).expect("completion stream");
        let first = stream.recv_timeout(Duration::from_secs(2)).expect("first chunk").expect("chunk");
        assert_eq!(first.len(), 1);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while host.stats().backpressure_events == 0 {
            assert!(std::time::Instant::now() < deadline, "backpressure not observed");
            thread::yield_now();
        }
        stream.cancel();
        drop(stream);
        let stats = host.stats();
        assert!(stats.backpressure_events > 0);
        assert_eq!(stats.active_requests, 0);
    }

    #[test]
    fn result_quota_is_enforced_on_stream_output() {
        let policy = ExtensionResourcePolicy { max_result_bytes: 1, chunk_candidates: 8, ..ExtensionResourcePolicy::default() };
        let host = host(policy);
        let stream = host.complete(completion(5)).expect("stream");
        assert!(matches!(stream.recv_timeout(Duration::from_secs(2)), Err(ExtensionError::ResultQuota { limit: 1 })));
    }

    #[test]
    fn fuel_trap_restarts_store_and_decoration_provider_still_works() {
        let host = host(ExtensionResourcePolicy { fuel_per_request: 10_000, ..ExtensionResourcePolicy::default() });
        host.exercise_cpu_quota_and_restart().expect("trap and restart");
        assert_eq!(host.stats().traps, 1);
        assert_eq!(host.stats().restarts, 1);
        let decorations = host
            .decorate(&DecorationRequest { document_id: DocumentId::new(1), document_revision: DocumentRevision::new(2), visible: vec![0..8, 10..12] })
            .expect("provider after restart");
        assert!(!decorations.is_empty());
    }

    #[test]
    fn client_and_workspace_placement_hosts_are_distinct() {
        let client = WasmExtensionHost::new(HostPlacement::Client, ExtensionResourcePolicy::default()).expect("client host");
        let workspace = WasmExtensionHost::new(HostPlacement::Workspace, ExtensionResourcePolicy::default()).expect("workspace host");
        assert_eq!(client.placement(), HostPlacement::Client);
        assert_eq!(workspace.placement(), HostPlacement::Workspace);
    }
}
