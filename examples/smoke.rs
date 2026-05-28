//! `examples/smoke.rs` — release-time smoke target.
//!
//! `release.yml` runs `cargo run --release --target <T> --example smoke`
//! on every build matrix entry. The intent is to catch regressions
//! that `cargo test` misses — class signature: the lib tests pass on
//! every runner, but the produced binary panics on real-world startup
//! (e.g., rustls `CryptoProvider` not pre-installed before the first
//! HTTPS handshake — shoka v0.10.0 shipped that exact bug).
//!
//! The default body is intentionally no-op so kata can drop this file
//! into every consumer crate without breaking releases that haven't
//! yet decided what to exercise. **Override this file** in each crate
//! to call the real startup path that's most likely to regress:
//!
//! - HTTPS-using CLIs: build the actual API client (octocrab,
//!   reqwest, etc.) and issue a tiny no-auth GET (e.g.,
//!   `https://api.github.com/zen`) — that forces the rustls handshake
//!   to run inside the same binary the release publishes.
//! - File-handling CLIs: write+read a temp file via the real I/O
//!   helpers (catches missing crate features, permission regressions).
//! - Library-only crates: just exit 0; `cargo test` is sufficient.
//!
//! Cost: an extra ~5-second job step per platform per release.
//! Payoff: when this fails, the release blocks before publishing to
//! GitHub Releases / crates.io, instead of users finding the bug.

fn main() {
    eprintln!(
        "smoke: no-op default — override examples/smoke.rs in this crate \
         to exercise the startup path most likely to regress (HTTPS \
         handshake, file I/O, etc.). See the file's module doc for ideas."
    );
}
