# Orbit

Orbit is an experimental WebAssembly isolate runtime for applications compiled with the
[Orin](https://github.com/orin-lang/orin-lang) programming language. Many independent modules run in one host
process on a shared Wasmtime engine; each instance has its own memory and state and can reach only the host
capabilities it was explicitly granted.

**Status:** milestone 1, not production-ready. Read [docs/security.md](docs/security.md) first.

```sh
cargo run -p orbit-cli -- inspect crates/orbit-cli/tests/fixtures/hello.wat
cargo run -p orbit-cli -- run crates/orbit-cli/tests/fixtures/hello.wat --grant log/v1 --invoke hello
cargo test --workspace
cargo bench -p orbit-runtime
```

| Crate | Role |
|---|---|
| `orbit-abi` | versioned host ABI: capability ids, signatures, status codes, manifest (no engine dependency) |
| `orbit-runtime` | shared engine, validated and cached artifacts, isolates, limits, admission control, metrics |
| `orbit-caps` | capabilities `log/v1`, `clock/v1`, `random/v1`, `kv/v1`, `queue/v1`, `fs/v1`, `process/v1`, the restricted WASI preview 1 shim `wasi_p1/v1`, and their adapters |
| `orbit-cli` | the `orbit` command (`run`, `inspect`) |

Documentation: [architecture](docs/architecture.md) · [host ABI](docs/host-abi.md) ·
[Orin integration](docs/orin-integration.md) · [security and limitations](docs/security.md) ·
[benchmarks](docs/benchmarks.md) · [roadmap](docs/roadmap.md) · [ADRs](docs/adr/README.md)
