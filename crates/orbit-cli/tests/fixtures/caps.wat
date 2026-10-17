(module
  (import "orbit:clock/v1" "now_unix_nanos" (func $now (result i64)))
  (import "orbit:clock/v1" "monotonic_nanos" (func $mono (result i64)))
  (import "orbit:random/v1" "fill" (func $fill (param i64 i64) (result i32)))
  (memory (export "memory") 1)
  (func (export "now_ms") (result i64) (i64.div_u (call $now) (i64.const 1000000)))
  (func (export "mono") (result i64) (call $mono))
  (func (export "fill") (result i32) (call $fill (i64.const 0) (i64.const 16))))
