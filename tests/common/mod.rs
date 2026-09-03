//! A fresh, empty `target/tmp/<test name>/<n>`, wiped on creation and left afterwards for
//! inspection.

use std::cell::Cell;
use std::path::PathBuf;
use std::{fs, thread};

pub fn fresh_dir() -> PathBuf {
    thread_local! {
        static CALLS: Cell<u32> = const { Cell::new(0) };
    }
    let test = thread::current()
        .name()
        .unwrap_or("unnamed")
        .replace("::", "/");
    let call = CALLS.with(|calls| calls.replace(calls.get() + 1));
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(test)
        .join(call.to_string());
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create test directory");
    path
}
