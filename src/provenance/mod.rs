//! Experimental versioned provenance with optional embedded public proof storage.
pub mod manifest;
pub mod conversion;
pub mod archive;
pub mod stages;
#[cfg(feature = "verification")]
pub mod trust;
#[cfg(feature = "verification")]
mod openssl_process;
#[cfg(feature = "verification")]
pub mod signing;
#[cfg(feature = "verification")]
pub mod verification;
#[cfg(feature = "verification")]
pub mod evidence;
#[cfg(feature = "verification")]
pub mod historical;
#[cfg(feature = "verification")]
pub mod embedded;
