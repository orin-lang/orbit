# Orin guest programs

Small Orin programs, compiled with the Orin compiler for Orbit and committed as `.wasm` fixtures in
`crates/orbit-cli/tests/fixtures/orin/` (`<name>_32.wasm` = `wasm32-orbit`, `<name>_64.wasm` = `wasm64-orbit`, both `--release`).
`crates/orbit-cli/tests/orin_e2e.rs` runs them. The Orin commit they were built with is in `fixtures/orin/VERSION`.

| Program | Shows |
|---|---|
| `hello` | `console.log` lines reach the log sink in order; exit status 0 |
| `exit` | `std:process.exit(7)`: `orin_main` traps, `orin_exit_status` = 7 |
| `trap` | out-of-bounds index: trap message on the error stream, `orin_exit_status` = 101 |
| `compute` | fib, sieve, collatz, strings; checksums equal the native build |
| `clock` | `std:time` through `orbit:clock/v1` (needs `clock/v1`) |
| `tasks` | `std:task` spawn/join and a channel, run one at a time inside the module |

`<name>.expected` is the stdout of the same program built natively by Orin (`orin build --release`).

Rebuild (needs an Orin checkout with `./tools/bootstrap.sh` done, `clang` with the WebAssembly backend, `wasm-ld`):

```sh
ORIN_DIR=/path/to/orin-lang tests/guests/orin/build.sh
```
