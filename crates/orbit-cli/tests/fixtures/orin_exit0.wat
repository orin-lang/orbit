;; Orin entry-point contract fixture (docs/orin-integration.md): wasm64 memory, orin_main,
;; returns 0.
(module
  (memory (export "memory") i64 1)
  (global $status (mut i32) (i32.const 0))
  (func (export "orin_main") (result i32) (i32.const 0))
)
