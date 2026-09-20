//! Build `brokey-setup.exe` from a binary and a package.
//!
//! ```sh
//! cargo run --release -p brokey --example make-setup -- \
//!     target/release/brokey.exe dist/brokey-0.1.4-x64.msi dist/brokey-setup-0.1.4-x64.exe
//! ```
//!
//! The setup executable is Brokey's own binary with the MSI appended and a
//! footer saying how long it is; `brokey_lib::setup::payload` has the format
//! and the reasoning.
//!
//! **A Rust example rather than a shell script**, unlike the rest of
//! `packaging/`, and for one reason: it calls the same `payload::append` the
//! running binary reads with, so the writer and the reader cannot drift.

use brokey_lib::setup::payload;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [executable, package, out] = args.as_slice() else {
        eprintln!("usage: make-setup <executable> <package.msi> <out.exe>");
        return std::process::ExitCode::FAILURE;
    };

    let exe = match std::fs::read(executable) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("could not read {executable}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let msi = match std::fs::read(package) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("could not read {package}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let setup = payload::append(&exe, &msi);
    // Read back before it is written, so a build that produced something the
    // installer cannot open fails here rather than on somebody's machine. It
    // costs one comparison of a several-megabyte slice.
    match payload::read(&setup) {
        Some(back) if back == msi.as_slice() => {}
        _ => {
            eprintln!("the package did not read back out of the setup binary");
            return std::process::ExitCode::FAILURE;
        }
    }

    if let Err(e) = std::fs::write(out, &setup) {
        eprintln!("could not write {out}: {e}");
        return std::process::ExitCode::FAILURE;
    }

    // And then ask the written file the question the running binary asks of
    // itself, which is a different question from the one above and is the one
    // that would otherwise go unchecked. `read` proves the format round-trips
    // in memory; `carried_by` is what decides, on a double-click, that this
    // executable is an installer rather than plain Brokey. A setup binary
    // that fails it is one that would open Brokey instead of installing it.
    if !payload::carried_by(std::path::Path::new(out)) {
        eprintln!(
            "{out} was written but is not recognised as an installer; it would \
             open Brokey instead of installing it"
        );
        return std::process::ExitCode::FAILURE;
    }
    println!(
        "{out}: {} bytes ({} of program, {} of package)",
        setup.len(),
        exe.len(),
        msi.len()
    );
    std::process::ExitCode::SUCCESS
}
