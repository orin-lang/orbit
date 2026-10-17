;; Orin entry-point contract fixture (docs/orin-integration.md): wasm64 memory, orin_main,
;; traps; orin_exit_status reports -1 (a genuine trap).
(module
  (memory (export "memory") i64 1)
  (global $status (mut i32) (i32.const -1))
  (func (export "orin_main") (result i32) unreachable)
  (func (export "orin_exit_status") (result i32) (global.get $status)))
