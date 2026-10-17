;; Orin entry-point contract fixture (docs/orin-integration.md): wasm64 memory, orin_main,
;; returns 256 (wraps to 0 mod 256, must become 1).
(module
  (memory (export "memory") i64 1)
  (global $status (mut i32) (i32.const 0))
  (func (export "orin_main") (result i32) (i32.const 256))
)
