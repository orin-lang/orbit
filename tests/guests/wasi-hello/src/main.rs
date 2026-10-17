//! Test guest for Orbit's restricted WASI preview 1 shim. The first argument selects what to do.

use std::io::Read;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    println!("hello from wasi");
    println!("args={args:?}");
    eprintln!("this goes to stderr");

    match args.get(1).map(String::as_str) {
        None => {}
        Some("exit") => {
            let code = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
            println!("exiting with {code}");
            std::process::exit(code);
        }
        Some("env") => {
            let mut vars: Vec<(String, String)> = std::env::vars().collect();
            vars.sort();
            println!("env={vars:?}");
        }
        Some("time") => {
            // Needs the clock: panics (-> trap) if `clock/v1` is not granted.
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            let t0 = Instant::now();
            println!("unix_secs={}", now.as_secs());
            println!("monotonic_ok={}", t0.elapsed().as_secs() < 5);
        }
        Some("random") => {
            // std seeds hash maps from `random_get`: panics if `random/v1` is not granted.
            let mut m = std::collections::HashMap::new();
            m.insert("k", 1);
            println!("hashmap_ok={}", m["k"] == 1);
        }
        Some("stdin") => {
            let mut s = String::new();
            let n = std::io::stdin().read_to_string(&mut s).unwrap();
            println!("stdin_bytes={n}");
        }
        Some("panic") => panic!("boom"),
        Some(other) => println!("unknown mode {other}"),
    }
    println!("done");
}
