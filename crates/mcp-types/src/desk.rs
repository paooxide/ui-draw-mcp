//! The hand-off between an engine and the desktop it may borrow under
//! `foreground = "polite"`.
//!
//! The window engine raises apps and the input engine moves the pointer, but
//! both borrow the same desktop and must give it back the same way: wait for
//! the person to be idle, remember the frontmost app and the pointer, and put
//! both back afterwards on every path. That logic needs an input backend, so it
//! lives in the input engine; this trait is how an engine that has no input
//! backend of its own asks for it.

use async_trait::async_trait;

use crate::{CancelToken, Envelope};

/// Something that can lend the desktop for the length of one call.
#[async_trait]
pub trait Desk: Send + Sync {
    /// Wait until the person is idle, then record the desktop. `Err` is the
    /// finished refusal to return to the caller (the person stayed busy, the
    /// platform cannot tell, the kill switch or a cancel arrived), and nothing
    /// has been done.
    async fn borrow(&self, tool: &str, cancel: &CancelToken) -> Result<Box<dyn Loan>, Envelope>;
}

/// A borrowed desktop. If it is dropped without [`Loan::settle`] the desktop
/// is put back all the same.
#[async_trait]
pub trait Loan: Send {
    /// Give the desktop back after the call that used it produced `outcome`:
    /// restore the frontmost app and the pointer, turn an action the person
    /// interrupted into a retryable "user active" error, and add what was
    /// restored to the result as `polite`.
    async fn settle(self: Box<Self>, outcome: Envelope) -> Envelope;
}
