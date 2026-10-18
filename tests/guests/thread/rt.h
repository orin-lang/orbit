/* Freestanding runtime for the thread guests: imports, atomics and the thread entry glue.
 * Built for wasm32 with -matomics -mbulk-memory, linked with --shared-memory --import-memory. */
#ifndef RT_H
#define RT_H

typedef unsigned int u32;
typedef long long i64;

#define IMPORT(mod, name) __attribute__((import_module(mod), import_name(name)))
#define EXPORT(name) __attribute__((export_name(name)))

IMPORT("orbit:thread/v1", "spawn") i64 orbit_spawn(i64 arg);
IMPORT("orbit:log/v1", "write") int orbit_log(int level, i64 ptr, i64 len);

/* memory.atomic.wait32 / notify. wait: 0 woken, 1 value mismatch, 2 timed out. */
static inline int wait32(u32 *addr, u32 expected, i64 timeout_ns) {
  return __builtin_wasm_memory_atomic_wait32((int *)addr, (int)expected, timeout_ns);
}
static inline u32 notify(u32 *addr, u32 count) {
  return __builtin_wasm_memory_atomic_notify((int *)addr, count);
}
static inline u32 ld(u32 *a) { return __atomic_load_n(a, __ATOMIC_SEQ_CST); }
static inline void st(u32 *a, u32 v) { __atomic_store_n(a, v, __ATOMIC_SEQ_CST); }
static inline u32 add(u32 *a, u32 v) { return __atomic_add_fetch(a, v, __ATOMIC_SEQ_CST); }

/* Blocks until *a == want (futex style, no timeout). */
static inline void wait_eq(u32 *a, u32 want) {
  for (;;) {
    u32 v = ld(a);
    if (v == want) return;
    wait32(a, v, -1);
  }
}

/* A thread's stack: each instance has its own `__stack_pointer` global that starts at the
 * main thread's stack, so the thread entry must switch to a private region first. */
#define MAX_THREADS 16
#define STACK_SIZE 16384
static char stacks[MAX_THREADS][STACK_SIZE] __attribute__((aligned(16)));

static inline void set_sp(void *p) {
  __asm__ volatile(".globaltype __stack_pointer, i32\n\tlocal.get %0\n\tglobal.set __stack_pointer"
                   :: "r"(p));
}

void thread_main(i64 tid, i64 arg);

EXPORT("orin_thread_start") void orin_thread_start(i64 tid, i64 arg) {
  set_sp(&stacks[(tid - 1) % MAX_THREADS][STACK_SIZE]);
  thread_main(tid, arg);
}

#endif
