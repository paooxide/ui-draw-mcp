//! Application lifecycle — install & uninstall.
//!
//! Its own category, not folded into `desktop`, because the risk profile is
//! categorically different: an install is arbitrary code execution *plus*
//! persistence, run by a manager that usually has elevated rights.
//!
//! Three independent switches stand between an agent and an install: the
//! `packages` category must be enabled, the exact tool must be named in
//! `policy.enable`, and (interactively) a human must approve the **resolved**
//! package — id, version and source as the manager resolved them, which can
//! differ from what was requested.

mod brew;
mod policy;
mod tools;

pub use policy::{is_protected, valid_package_id, PkgPolicy, Refusal, PROTECTED};
pub use tools::PkgModule;
