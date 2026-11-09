//! EdgeForge — on-device AI application lifecycle manager.
//!
//! The crate is split into small, independently testable pieces:
//!
//! * [`oci`] / [`pack`] — OCI image-layout artifacts and SHA-256 verification
//! * [`fetch`] — artifact download with timeouts, retries and truncation checks
//! * [`manager`] — lifecycle operations, A/B slots, health-gated updates and rollback
//! * [`process`] / [`health`] — process supervision and health probes
//! * [`cgroup`] — cgroups v2 CPU / memory limits
//! * [`metrics`] / [`api`] — Prometheus exposition and the HTTP control API

pub mod api;
pub mod cgroup;
pub mod error;
pub mod fetch;
pub mod fsutil;
pub mod health;
pub mod manager;
pub mod metrics;
pub mod oci;
pub mod pack;
pub mod process;
pub mod spec;

pub use error::{EdgeError, Result};
pub use manager::{AppStatus, Health, Manager, ManagerConfig, Slot};
pub use spec::AppSpec;
