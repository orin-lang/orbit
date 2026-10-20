/* A listener opened by main, accepted and read by a thread. */
#include "rt.h"

IMPORT("orbit:net/v1", "tcp_listen") i64 net_listen(i64 ap, i64 al, int port, int backlog, int opt);
IMPORT("orbit:net/v1", "local_port") int net_local_port(i64 h);
IMPORT("orbit:net/v1", "tcp_accept") i64 net_accept(i64 l, int timeout_ms, i64 wake);
IMPORT("orbit:net/v1", "tcp_connect") i64 net_connect(i64 hp, i64 hl, int port, int timeout_ms, i64 wake);
IMPORT("orbit:net/v1", "recv") i64 net_recv(i64 h, i64 p, i64 cap, int timeout_ms, i64 wake);
IMPORT("orbit:net/v1", "send") i64 net_send(i64 h, i64 p, i64 len, int timeout_ms, i64 wake);
IMPORT("orbit:net/v1", "close") int net_close(i64 h);

static const char loopback[] = "127.0.0.1";
static const char ping[] = "ping";
static char buf[64];
static u32 done, got;

void thread_main(i64 tid, i64 listener) {
  i64 s = net_accept(listener, 10000, 0);
  if (s > 0) {
    i64 n = net_recv(s, (i64)(u32)buf, sizeof buf, 10000, 0);
    if (n > 0) st(&got, (u32)n);
    net_send(s, (i64)(u32)"pong", 4, 10000, 0);
    net_close(s);
  }
  st(&done, 1);
  notify(&done, 1);
}

/* Returns the number of bytes the thread received (4), or a negative step number. */
EXPORT("serve_in_thread") int serve_in_thread(void) {
  i64 l = net_listen((i64)(u32)loopback, 9, 0, 16, 1);
  if (l <= 0) return -1;
  int port = net_local_port(l);
  if (orbit_spawn(l) <= 0) return -2;
  i64 c = net_connect((i64)(u32)loopback, 9, port, 10000, 0);
  if (c <= 0) return -3;
  net_send(c, (i64)(u32)ping, 4, 10000, 0);
  char reply[8];
  i64 n = net_recv(c, (i64)(u32)reply, sizeof reply, 10000, 0);
  if (n != 4 || reply[0] != 'p') return -4;
  wait_eq(&done, 1);
  /* The listener main opened is still usable by main after the thread used it. */
  net_close(c);
  net_close(l);
  return (int)ld(&got);
}
