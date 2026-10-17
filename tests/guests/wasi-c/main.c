/*
 * Test guest for Orbit's restricted WASI preview 1 shim, built with clang against wasi-libc.
 *
 * There is no wasi sysroot (headers) on the machine that built the committed fixture, so the
 * few libc functions used are declared by hand and linked against the self-contained wasi-libc
 * that ships with Rust's wasm32-wasip1 target (see ../build.sh). With a full wasi-sdk you would
 * simply #include <stdio.h> and write `int main(int argc, char **argv)`.
 */
extern int printf(const char *, ...);
extern char *getenv(const char *);
extern void exit(int);
extern long long time(long long *);
extern int atoi(const char *);

/* clang 18 keeps the name `main` for main(argc, argv); this libc's __main_void calls
 * __main_argc_argv, so name the symbol explicitly. */
int app_main(int argc, char **argv) __asm__("__main_argc_argv");

int app_main(int argc, char **argv) {
  printf("c hello argc=%d\n", argc);
  for (int i = 0; i < argc; i++) printf("argv[%d]=%s\n", i, argv[i]);
  const char *v = getenv("GREETING");
  printf("GREETING=%s\n", v ? v : "(unset)");
  if (argc > 2 && argv[1][0] == 'e') exit(atoi(argv[2]));
  if (argc > 1 && argv[1][0] == 't') printf("time=%lld\n", time(0));
  return 0;
}
