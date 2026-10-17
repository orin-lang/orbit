;; Imports a WASI function the shim does not implement: rejected at load.
(module
  (import "wasi_snapshot_preview1" "path_open"
    (func (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32))))
