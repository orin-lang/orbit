;; A WASI command: `_start` writes a line to stdout and exits with code 9.
(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "\10\00\00\00\07\00\00\00")
  (data (i32.const 16) "command")
  (func (export "_start")
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 100)))
    (call $proc_exit (i32.const 9)))
  (func (export "helper") (result i32) (i32.const 5)))
