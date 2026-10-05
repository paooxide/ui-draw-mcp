//! Browser engine: DOM-level control of a Chromium browser over the Chrome
//! DevTools Protocol. Unlike the desktop
//! engines this crate is OS-independent (it speaks CDP over TCP), so the real
//! backend ([`CdpBackend`]) ships here, not in a per-OS crate.

mod backend;
mod branch;
mod cdp;
pub mod challenge;
mod checkpoint;
pub mod flow;
mod nav;
mod profile;
pub mod record;
pub mod safari;
mod screencast;
pub mod showcase;
mod tools;
mod visual;

pub use backend::{
    ActOpts, BrowserBackend, BrowserError, CdpBackend, EvalOptions, Locator, ScrollMode, Shot,
    CHROME_BINS,
};
pub use branch::{Branch, BranchError, BranchManager, BranchStatus};
pub use cdp::{DialogPolicy, RecordDialogs};
pub use challenge::{ChallengeKind, ChallengeManager, ChallengeStatus};
pub use checkpoint::{Checkpoint, CheckpointStore, FormInputState};
pub use flow::{Flow, FlowError, FlowStore};
pub use nav::{NavDenied, NavPolicy};
pub use profile::{Profile, ProfileError, ProfileStore};
pub use record::{MacroSynthesizer, RawInteractionEvent, RecordManager};
pub use safari::{
    find_safaridriver, is_safari_available, SafariDriverConfig, SafariProcess, SafariSession,
};
pub use screencast::ScreencastOpts;
pub use showcase::{CursorStyle, ShowcaseConfig, ShowcaseSpeed};
pub use tools::BrowserModule;
pub use visual::{Baseline, VisualStore};
