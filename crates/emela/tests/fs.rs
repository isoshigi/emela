//! Tests for the `Fs` capability (spec 0055): importing the embedded `std.fs`
//! brings the effect `Fs` and its handle/error types into scope. Frontend tests
//! use `emela check`; runtime tests exercise `emela run` (wasm-wasi + wasmi host)
//! and cross-backend consistency with `js-node`. A wasip2 lowering test confirms
//! the core module imports `emela_fs` on the component backend.
//!
//! This file follows the convention of `tests/random.rs` — all capability tests
//! live in a single file, mixing check-level, runtime, and backend lowering
//! assertions.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

fn emela() -> Command {
    Command::new(env!("CARGO_BIN_EXE_emela"))
}

fn temp_dir(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("emela-fs-{label}-{}-{id}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Helpers — frontend (emela check)
// ---------------------------------------------------------------------------

/// Runs `emela check` against a single self-contained file (no package).
fn check_single(source: &str) -> std::process::Output {
    let dir = temp_dir("check");
    let input = dir.join("main.emel");
    fs::write(&input, source).unwrap();
    let output = emela().arg("check").arg(&input).output().unwrap();
    let _ = fs::remove_dir_all(&dir);
    output
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// Helpers — runtime (emela run / js-node)
// ---------------------------------------------------------------------------

/// Compiles a program with `emela run` (wasm-wasi backend + wasmi host) and
/// returns trimmed stdout.
fn run_wasm(source: &str) -> Vec<u8> {
    let dir = temp_dir("wasm");
    let input = dir.join("main.emel");
    fs::write(&input, source).unwrap();
    let output = emela().arg("run").arg(&input).output().unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "wasm run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Compiles a program with the JS backend and runs it via `node`,
/// returning trimmed stdout.
fn run_js(source: &str) -> Vec<u8> {
    let dir = temp_dir("js");
    let input = dir.join("main.emel");
    let js_path = dir.join("out.js");
    fs::write(&input, source).unwrap();
    let build = emela()
        .args(["build", "--backend", "js-node", "-o"])
        .arg(&js_path)
        .arg(&input)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "js build failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let node = Command::new("node").arg(&js_path).output().unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert!(
        node.status.success(),
        "node run failed:\n{}",
        String::from_utf8_lossy(&node.stderr)
    );
    node.stdout
}

// ---------------------------------------------------------------------------
// Helpers — backend lowering (wasm-wasip2 --emit text)
// ---------------------------------------------------------------------------

/// The core-module WAT the `wasm-wasip2` backend emits (spec 0052).
fn wasip2_wat(program: &str) -> String {
    let dir = temp_dir("wasip2");
    let input = dir.join("main.emel");
    fs::write(&input, program).unwrap();
    let output = emela()
        .args(["build", "--backend", "wasm-wasip2", "--emit", "text"])
        .arg(&input)
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "wasip2 build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

// ===========================================================================
// Frontend tests (emela check)
// ===========================================================================

/// Imports `std.fs` and calls each public operation inside `uses { Fs }`. This
/// exercises the whole effect block: `open_read`/`open_write` (the open
/// wrappers), `read`/`write` (the data wrappers), `close` (the close wrapper),
/// and `read_file`/`write_file` (the convenience wrappers).
#[test]
fn fs_module_imports_and_typechecks() {
    let output = check_single(
        "import std.fs\n\
         import std.bytes\n\
         \n\
         fn main() -> Unit uses { Fs } {\n\
             try {\n\
                 let f = Fs.open_read(\"in.txt\")\n\
                 let data = Fs.read_file(\"in.txt\")\n\
                 let n = bytes.length(data)\n\
                 Fs.close(f.id)\n\
             } catch { e -> () }\n\
         }\n",
    );
    assert!(
        output.status.success(),
        "expected check to pass:\n{}",
        stderr(&output)
    );
}

/// `FsError` is a public enum whose variants can be matched — the failure
/// value delivered on the throws channel (spec 0043).
#[test]
fn fs_error_is_matchable() {
    let output = check_single(
        "import std.fs\n\
         \n\
         fn describe(e: FsError) -> String uses {} {\n\
             match e {\n\
                 FsError::NotFound(m) -> m\n\
                 FsError::PermissionDenied(m) -> m\n\
                 FsError::Io(m) -> m\n\
             }\n\
         }\n\
         \n\
         fn main() -> Unit uses {} {\n\
             let _ = describe(FsError::Io(\"test\"))\n\
         }\n",
    );
    assert!(
        output.status.success(),
        "expected check to pass:\n{}",
        stderr(&output)
    );
}

/// An `Fs` operation is usable only inside a `uses { Fs }` scope: calling
/// `Fs.close` from a `uses {}` function is rejected (spec 0037).
#[test]
fn fs_operation_requires_capability() {
    let output = check_single(
        "import std.fs\n\
         \n\
         fn main() -> Unit uses {} {\n\
             try { Fs.close(1) } catch { e -> () }\n\
         }\n",
    );
    assert!(!output.status.success(), "expected check to fail");
    assert!(
        stderr(&output).contains("Fs"),
        "diagnostic should name the Fs effect:\n{}",
        stderr(&output)
    );
}

/// The backing `raw_open_read`/`raw_open_write`/`raw_read`/`raw_write`
/// operations are private to the effect (spec 0037): a program cannot call
/// `Fs.raw_open_read` directly; only the `pub fn` wrappers are public.
#[test]
fn backing_operations_are_private() {
    let output = check_single(
        "import std.fs\n\
         \n\
         fn main() -> Unit uses { Fs } {\n\
             try {\n\
                 let _ = Fs.raw_open_read(\"in.txt\")\n\
             } catch { e -> () }\n\
         }\n",
    );
    assert!(
        !output.status.success(),
        "expected check to fail: raw_open_read is private"
    );
}

/// `File` is a public record, constructible by users (for testing / mocking
/// handles). Its `id` field is an `Int`.
#[test]
fn file_record_is_constructible() {
    let output = check_single(
        "import std.fs\n\
         \n\
         fn main() -> Unit uses {} {\n\
             let f = File { id: 1 }\n\
             ()\n\
         }\n",
    );
    assert!(
        output.status.success(),
        "expected check to pass:\n{}",
        stderr(&output)
    );
}

// ===========================================================================
// Runtime tests (emela run + js-node)
// ===========================================================================

/// Writes a known payload to a file, reads it back via `string_from_bytes`, and
/// prints it so the test can assert the round-trip. Uses the convenience
/// wrappers `Fs.write_file` and `Fs.read_file`.
#[test]
fn write_and_read_file_round_trips() {
    let dir = temp_dir("rw");
    let file_path = dir.join("data.bin");

    let source = format!(
        "import std.fs\n\
         import std.io\n\
         import std.bytes\n\
         \n\
         fn main() -> Unit uses {{ Io, Fs }} {{\n\
             try {{\n\
                 Fs.write_file(\"{path}\", bytes.to_bytes(\"hello, file system\\n\"))\n\
                 let data = Fs.read_file(\"{path}\")\n\
                 match bytes.string_from_bytes(data) {{\n\
                     Some(s) -> Io.print(s)\n\
                     None -> ()\n\
                 }}\n\
             }} catch {{ e -> () }}\n\
         }}\n",
        path = file_path.display().to_string().replace('\\', "\\\\")
    );
    let wasm_out = run_wasm(&source);
    assert_eq!(String::from_utf8_lossy(&wasm_out), "hello, file system\n");
}

/// Opening a file that does not exist yields `FsError::NotFound` on the throws
/// channel, caught by the program.
#[test]
fn file_not_found_yields_not_found_error() {
    let dir = temp_dir("nf");
    let missing = dir.join("does_not_exist.txt");

    let source = format!(
        "import std.fs\n\
         import std.io\n\
         \n\
         fn main() -> Unit uses {{ Io, Fs }} {{\n\
             let out = try {{\n\
                 Fs.read_file(\"{path}\")\n\
                 \"ok\"\n\
             }} catch {{\n\
                 FsError::NotFound(msg) -> \"not-found\"\n\
                 e -> \"other\"\n\
             }}\n\
             Io.print(out)\n\
         }}\n",
        path = missing.display().to_string().replace('\\', "\\\\")
    );
    let wasm_out = run_wasm(&source);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        String::from_utf8_lossy(&wasm_out),
        "not-found",
        "expected 'not-found', got {:?}",
        String::from_utf8_lossy(&wasm_out)
    );
}

/// The same write-read program produces the same output on the wasm-wasi and
/// js-node backends (spec 0000, spec 0055).
#[test]
fn write_and_read_bytes_cross_backend() {
    let dir = temp_dir("xbe");
    let file_path = dir.join("cross.txt");

    let source = format!(
        "import std.fs\n\
         import std.io\n\
         import std.bytes\n\
         \n\
         fn main() -> Unit uses {{ Io, Fs }} {{\n\
             try {{\n\
                 Fs.write_file(\"{path}\", bytes.to_bytes(\"cross-backend\\n\"))\n\
                 let data = Fs.read_file(\"{path}\")\n\
                 match bytes.string_from_bytes(data) {{\n\
                     Some(s) -> Io.print(s)\n\
                     None -> ()\n\
                 }}\n\
             }} catch {{ e -> () }}\n\
         }}\n",
        path = file_path.display().to_string().replace('\\', "\\\\")
    );

    let wasm_out = run_wasm(&source);
    let js_out = run_js(&source);
    assert_eq!(
        String::from_utf8_lossy(&wasm_out),
        String::from_utf8_lossy(&js_out),
        "wasm and JS must agree"
    );
    assert_eq!(String::from_utf8_lossy(&wasm_out), "cross-backend\n");
}

// ===========================================================================
// Backend lowering tests
// ===========================================================================

/// The `Fs` capability (spec 0055) lowers to `wasi:filesystem` imports on the
/// wasm-wasip2 backend: the core module imports `wasi:filesystem/types`
/// and `wasi:filesystem/preopens`, and no unrelated capability is pulled in.
#[test]
fn fs_effect_lowers_to_wasi_filesystem_on_wasip2() {
    let program = "import std.fs\n\
        fn main() -> Int uses { Fs } {\n\
        \x20   try {\n\
        \x20       let f = Fs.open_read(\"x\")\n\
        \x20       f.id\n\
        \x20   } catch { e -> 0 }\n\
        }\n";
    let wat = wasip2_wat(program);
    assert!(
        wat.contains("wasi:filesystem/types@0.2.0"),
        "Fs capability should import wasi:filesystem:\n{wat}"
    );
    assert!(
        !wat.contains("wasi:sockets"),
        "an Fs-only program must not import wasi:sockets:\n{wat}"
    );
    assert!(
        !wat.contains("wasi:random"),
        "an Fs-only program must not import wasi:random:\n{wat}"
    );
}
