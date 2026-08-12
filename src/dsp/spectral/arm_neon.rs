//! ARM64/NEON backend — the reserved slot, not yet an implementation.
//!
//! The dispatch architecture is deliberately shaped so this lands the same
//! way the x86 backend did: implement the five [`Kernels`] entries with NEON
//! intrinsics (`core::arch::aarch64`), sharing
//! [`LOG2_POLY`](super::scalar::LOG2_POLY) and the scalar remainder helpers,
//! add the table here, and extend [`super::dispatch::select`] with an
//! `#[cfg(target_arch = "aarch64")]` arm checking
//! `std::arch::is_aarch64_feature_detected!("neon")` (NEON is baseline on
//! AArch64, so that arm may simply return the table unconditionally).
//!
//! Until then, AArch64 builds run the scalar backend, which is correct by
//! construction — it is the reference the SIMD parity tests compare against.
//!
//! Nothing outside `dispatch.rs` may name an architecture: the detector only
//! ever sees the [`Kernels`] table, which is what keeps this a drop-in.
//!
//! [`Kernels`]: super::dispatch::Kernels

// Intentionally empty of code.
