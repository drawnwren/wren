# wren-provider

Restartable client-provider process and bounded protocol. Open-document actors
provide revisioned native/lexical highlighting and word completion outside the
input/presenter failure boundary. The crate also owns latest-wins demand queues,
freshness validation, atomic completion acceptance, fuzzy picker matching, and
decoration mapping/hiding rules.

GPU compute is default-enabled for data-parallel lexical classification of
large demanded ASCII ranges when a bundled native parser has no result. The
provider asks for a high-performance hardware `wgpu` adapter lazily and reuses
storage/readback buffers across demands. Inputs below the measured 4 MiB
crossover, software adapters, non-ASCII input, device or mapping failures, and
device limits transparently retain the existing CPU lexer. Native Tree-sitter
parsing and the input/layout/presenter hot path stay on CPU because they are not
profitable data-parallel GPU workloads. Build with `--no-default-features` to
omit the GPU backend entirely.

The comparative benchmark verifies byte-identical provider responses before
timing 4 MiB, 8 MiB, and 32 MiB generated workloads and reports throughput for
both backends:

```sh
cargo bench -p wren-provider --bench provider_acceleration
```

When no hardware GPU is available, it records CPU baselines only instead of
presenting a software renderer as GPU acceleration.

The macOS hardware-GPU CI job retains the Criterion report and requires at
least `1.5x` median speedup for every workload with:

```sh
python3 scripts/check-provider-gpu-speedup.py \
  target/criterion/provider-lexical-classification \
  --minimum-speedup 1.5
```
