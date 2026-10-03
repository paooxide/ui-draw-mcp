//! The Linux desktop backend.
//!
//! Perception reads the AT-SPI2 accessibility bus, the same tree a screen
//! reader uses, so an action targets a real widget rather than a pixel.
//! Synthetic input goes through the `RemoteDesktop` portal, which is the only
//! route a Wayland compositor allows and which asks the human once, in a
//! dialog the agent cannot see or answer. Capture goes through the
//! `Screenshot` portal. Session state (lock, idle, settings, media, power)
//! is D-Bus.
//!
//! GNOME on Wayland is what this was written against and validated on. The
//! AT-SPI half is toolkit-level and works anywhere the a11y bus runs; the
//! portal half works on any compositor that implements the portals (GNOME,
//! KDE, wlroots with the right backend). Where GNOME offers something and
//! the portal spec does not (display geometry, idle time, the lock screen),
//! the GNOME D-Bus interface is used and the error names what is missing
//! elsewhere.
//!
//! On non-Linux targets this crate is empty.

#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
mod a11y;
#[cfg(target_os = "linux")]
mod backend;
#[cfg(target_os = "linux")]
mod clip;
#[cfg(target_os = "linux")]
mod desktop;
#[cfg(target_os = "linux")]
mod doctor;
#[cfg(target_os = "linux")]
mod image;
#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "linux")]
mod keys;
#[cfg(target_os = "linux")]
mod launch;
// Pure logic, so it is also compiled for tests on other hosts.
#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod pointer;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "linux")]
mod roles;
#[cfg(target_os = "linux")]
mod vision;
#[cfg(target_os = "linux")]
mod window;

#[cfg(target_os = "linux")]
pub use backend::LinuxBackend;
#[cfg(target_os = "linux")]
pub use desktop::LinuxDesktop;
#[cfg(target_os = "linux")]
pub use doctor::{doctor, Doctor};
