/* Guest for crates/orbit-runtime/tests/threads.rs. Every scenario is an export. */
#include "rt.h"

static u32 counter, done, flag, result, release, never, exit_status = 0xffffffffu, secret;
static u32 last_status, started;

enum { M_COUNT = 1, M_WAIT = 2, M_BLOCK = 3, M_TRAP = 4, M_EXIT = 5, M_SPIN = 6, M_RELEASE = 7,
       M_NOP = 8, M_LOG = 9 };

static const char hello[] = "hello from a guest thread";

void thread_main(i64 tid, i64 arg) {
  add(&started, 1);
  notify(&started, 1u << 30);
  /* The scenario is in the low byte of `arg`, a count in the rest: it cannot race with main. */
  u32 mode = (u32)(arg & 0xff);
  arg >>= 8;
  switch (mode) {
  case M_COUNT:
    for (i64 i = 0; i < arg; i++) add(&counter, 1);
    break;
  case M_WAIT:
    wait_eq(&flag, 1);
    st(&result, 42);
    notify(&result, 1u << 30);
    break;
  case M_BLOCK:
    wait32(&never, 0, -1); /* sleeps until the host wakes it */
    break;
  case M_TRAP:
    wait_eq(&started, 3); /* let the blockers start first */
    __builtin_trap();
  case M_EXIT:
    st(&exit_status, 7);
    __builtin_trap();
  case M_SPIN:
    for (volatile u32 i = 0;; i++) {}
  case M_RELEASE:
    wait_eq(&release, 1);
    break;
  case M_LOG:
    orbit_log(2, (i64)(u32)hello, sizeof hello - 1);
    break;
  default:
    break;
  }
  add(&done, 1);
  notify(&done, 1u << 30);
}

static void join_all(u32 n) { wait_eq(&done, n); }

/* n threads each add `iters` to one atomic counter. */
EXPORT("count") int count(int n, int iters) {
  u32 base = ld(&done); /* `done` and `counter` keep their values across invocations */
  for (int i = 0; i < n; i++) {
    if (orbit_spawn(M_COUNT | ((i64)iters << 8)) <= 0) return -1;
  }
  join_all(base + (u32)n);
  return (int)ld(&counter);
}

/* A thread waits for `flag`; main sets it and waits for the answer. */
EXPORT("wait_notify") int wait_notify(void) {
  if (orbit_spawn(M_WAIT) <= 0) return -1;
  st(&flag, 1);
  notify(&flag, 1u << 30);
  wait_eq(&result, 42);
  join_all(1);
  return (int)ld(&result);
}

/* Spawns n threads that wait for `release`; returns how many spawns succeeded. */
EXPORT("spawn_n") int spawn_n(int n) {
  int ok = 0;
  for (int i = 0; i < n; i++) {
    i64 r = orbit_spawn(M_RELEASE);
    if (r > 0) ok++; else st(&last_status, (u32)r);
  }
  st(&release, 1);
  notify(&release, 1u << 30);
  join_all((u32)ok);
  return ok;
}
EXPORT("last_status") int get_last_status(void) { return (int)ld(&last_status); }

/* Two blocked threads and one that traps; main blocks forever too. */
EXPORT("trap_with_blockers") int trap_with_blockers(void) {
  orbit_spawn(M_BLOCK);
  orbit_spawn(M_BLOCK);
  orbit_spawn(M_TRAP);
  wait32(&never, 0, -1);
  return 0;
}

/* A thread records status 7 and traps (what orin_platform_exit does); main blocks. */
EXPORT("exit_in_thread") int exit_in_thread(void) {
  orbit_spawn(M_EXIT);
  wait32(&never, 0, -1);
  return 0;
}
EXPORT("orin_exit_status") int orin_exit_status(void) { return (int)ld(&exit_status); }

/* A thread spins forever; main blocks forever. */
EXPORT("spin_thread") int spin_thread(void) {
  orbit_spawn(M_SPIN);
  wait32(&never, 0, -1);
  return 0;
}

/* Main returns while a thread is still blocked: the host must end it. */
EXPORT("leave_thread_behind") int leave_thread_behind(void) {
  orbit_spawn(M_BLOCK);
  wait_eq(&started, 1);
  return 5;
}

/* memory.grow by `pages`: the old size in pages, or -1. */
EXPORT("grow") int grow(int pages) { return (int)__builtin_wasm_memory_grow(0, pages); }
EXPORT("size") int size(void) { return (int)__builtin_wasm_memory_size(0); }

EXPORT("poke") void poke(int v) { st(&secret, (u32)v); }
EXPORT("peek") int peek(void) { return (int)ld(&secret); }

/* Host calls from main and from a thread (the log through the shared memory). */
EXPORT("log_main") int log_main(void) {
  return orbit_log(2, (i64)(u32)hello, sizeof hello - 1);
}
EXPORT("log_thread") int log_thread(void) {
  orbit_spawn(M_LOG);
  join_all(1);
  return 0;
}

/* One message from main and one from a thread in the same invocation (one shared budget). */
EXPORT("log_both") int log_both(void) {
  orbit_log(2, (i64)(u32)hello, sizeof hello - 1);
  orbit_spawn(M_LOG);
  join_all(1);
  return 0;
}

/* Spawns one thread that does nothing and waits for it (spawn latency). */
EXPORT("spawn_join") int spawn_join(int n) {
  for (int i = 0; i < n; i++) {
    u32 before = ld(&done);
    if (orbit_spawn(M_NOP) <= 0) return -1;
    wait_eq(&done, before + 1);
  }
  return (int)ld(&done);
}
EXPORT("spawn_once") i64 spawn_once(void) {
  return orbit_spawn(M_NOP);
}
