//! Helpers shared by the live-browser test files.
//!
//! Each file includes this with `mod common;`, so an item one file does not use
//! is not dead code to the others.
#![allow(dead_code)]

use std::future::Future;
use std::time::{Duration, Instant};

/// Poll `cond` until it returns true, panicking with `what` once `deadline`
/// has passed. Returns as soon as the condition holds, so a fast machine pays
/// nothing and a slow one gets the whole window instead of a guessed sleep.
pub async fn wait_until<F, Fut>(what: &str, deadline: Duration, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let end = Instant::now() + deadline;
    loop {
        if cond().await {
            return;
        }
        if Instant::now() >= end {
            panic!("timed out after {deadline:?} waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
