//! Test-only future driver.
//!
//! ADR 0010 forbids a dev-dependency on this crate, so its tests cannot use a
//! runtime attribute macro. They use this instead: a bounded poll loop over
//! [`Waker::noop`], which needs no executor, no thread, and — unlike a
//! hand-written `RawWaker` — no `unsafe`, so the crate keeps
//! `#![forbid(unsafe_code)]` in test builds too.
//!
//! Every future this crate constructs is driven purely by its own state
//! machine: the transitions are provider calls, and the test providers are
//! synchronous. Nothing ever parks waiting for an external wakeup, so a
//! no-op waker is sufficient and a `Pending` result only ever means "the
//! caller handed us a future this crate is not designed to drive". The poll
//! budget turns that mistake into a fast, explicit failure instead of a hung
//! test.

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

/// Polls at most this many times before declaring the future stalled.
const MAX_POLLS: u32 = 1_000_000;

/// Drives `future` to completion on the calling thread.
///
/// # Panics
///
/// Panics when the future is still `Pending` after [`MAX_POLLS`] polls, which
/// means it is waiting on something outside this crate.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..MAX_POLLS {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::yield_now();
    }
    panic!("future did not complete within {MAX_POLLS} polls: arcen-outputs embeds no executor");
}

#[cfg(test)]
mod tests {
    use super::block_on;

    #[test]
    fn drives_a_ready_future() {
        assert_eq!(block_on(core::future::ready(7_u8)), 7);
    }

    #[test]
    fn drives_a_multi_stage_async_block() {
        let output = block_on(async {
            let first = core::future::ready(2_u8).await;
            let second = core::future::ready(3_u8).await;
            first * second
        });
        assert_eq!(output, 6);
    }

    #[test]
    fn drives_a_future_that_returns_pending_once() {
        let mut polled = false;
        let output = block_on(core::future::poll_fn(move |context| {
            if polled {
                core::task::Poll::Ready(1_u8)
            } else {
                polled = true;
                context.waker().wake_by_ref();
                core::task::Poll::Pending
            }
        }));
        assert_eq!(output, 1);
    }
}
