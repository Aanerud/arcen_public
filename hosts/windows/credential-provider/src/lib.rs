//! Arcen's additive Windows Credential Provider.
//!
//! The platform-independent field, account, registration, and secret-handling
//! logic is built on every host. The COM server itself is only compiled for
//! Windows.

pub mod fields;
pub mod registration;
pub mod secret;
pub mod serialization;

pub mod provider;

#[cfg(windows)]
mod com;
#[cfg(windows)]
mod credential;
#[cfg(windows)]
mod factory;
#[cfg(windows)]
pub mod guid;
#[cfg(windows)]
mod log;
#[cfg(windows)]
mod pipe;

#[cfg(windows)]
pub use com::{DllCanUnloadNow, DllGetClassObject};
