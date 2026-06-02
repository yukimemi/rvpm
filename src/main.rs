//! Thin binary entry point.
//!
//! All of rvpm's logic lives in the `rvpm` library crate (`src/lib.rs`) so it
//! can be unit-tested, doc-tested, and embedded by other crates. This file is
//! just the binary shell that hands control to [`rvpm::run`] (#176).

fn main() -> anyhow::Result<()> {
    rvpm::run()
}
