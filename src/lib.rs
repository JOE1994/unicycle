#![deny(missing_docs)]
//! A container for an unordered collection of [Future]s.
//! This provides an experimental variant of `FuturesUnordered` aimed to be
//! _fairer_. Easier to maintain, and store the futures being polled in a way which
//! provides better memory locality.
//!
//! ## Architecture
//!
//! The `Unordered` type stores all futures being polled in a `PinSlab`. This [slab]
//! maintains a growable collection of fixed-size memory regions, allowing it to
//! store immovable objects. The primary feature of a slab is that it automatically
//! reclaims memory at low cost. Each future inserted into the slab is asigned an
//! _index_.
//!
//! Next to the futures we maintain two bitsets, one _active_ and one
//! _alternate_. When a future is woken up, the bit associated with its index is
//! set in the _active_ set, and the waker associated with the poll to `Unordered`
//! is called.
//!
//! Once `Unordered` is polled, it atomically swaps the _active_ and _alternate_
//! bitsets, waits until it has exclusive access to the now _alternate_ bitset, and
//! drains it from all the indexes which have been flagged to determine which
//! futures to poll.
//!
//! We can also add futures to `Unordered`, this is achieved by inserting it into
//! the slab, then marking that index in a special `pollable` collection that it
//! should be polled the next time `Unordered` is.
//!
//! [slab]: https://github.com/carllerche/slab
//!
//! ## Examples
//!
//! ```rust,no_run
//! use tokio::{stream::StreamExt as _, time};
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut futures = unicycle::Unordered::new();
//!
//!     futures.push(time::delay_for(Duration::from_secs(2)));
//!     futures.push(time::delay_for(Duration::from_secs(3)));
//!     futures.push(time::delay_for(Duration::from_secs(1)));
//!
//!     while let Some(_) = futures.next().await {
//!         println!("tick");
//!     }
//!
//!     println!("done!");
//! }
//! ```

use self::pin_slab::PinSlab;
use self::wake_set::{SharedWakeSet, WakeSet};
use self::waker::SharedWaker;
use futures_core::Stream;
use std::{
    collections::VecDeque,
    future::Future,
    iter, mem,
    pin::Pin,
    ptr,
    sync::Arc,
    task::{Context, Poll},
};

pub use self::bit_set::BitSet;

mod bit_set;
mod pin_slab;
mod wake_set;
mod waker;

/// Data that is shared across all sub-tasks.
struct Shared {
    /// The currently registered parent waker.
    waker: SharedWaker,
    /// The currently registered wake set.
    wake_set: SharedWakeSet,
}

impl Shared {
    /// Construct new shared data.
    fn new() -> Self {
        Self {
            waker: SharedWaker::new(),
            wake_set: SharedWakeSet::new(),
        }
    }
}

/// A container for an unordered collection of [Future]s.
pub struct Unordered<F>
where
    F: Future,
{
    // Indexes that needs to be polled after they have been added.
    pollable: Vec<usize>,
    // Slab of futures being polled.
    // They need to be pinned on the heap, since the slab might grow to
    // accomodate more futures.
    slab: PinSlab<F>,
    // The largest index inserted into the slab so far.
    max_index: usize,
    // Shared parent waker.
    // Includes the current wake target. Each time we poll, we swap back and
    // forth between this and `wake_alternate`.
    shared: Arc<Shared>,
    // Alternate wake set, used for growing the existing set when futures are
    // added. This is then swapped out with the active set to receive polls.
    wake_alternate: *mut WakeSet,
    // Pending outgoing results. Uses a queue to avoid interrupting polling.
    results: VecDeque<F::Output>,
}

unsafe impl<F> Send for Unordered<F> where F: Future {}
unsafe impl<F> Sync for Unordered<F> where F: Future {}

impl<F> Unpin for Unordered<F> where F: Future {}

impl<F> Unordered<F>
where
    F: Future,
{
    /// Construct a new, empty [Unordered].
    pub fn new() -> Self {
        let alternate = WakeSet::new();
        alternate.lock_write();

        Self {
            pollable: Vec::with_capacity(16),
            slab: PinSlab::new(),
            max_index: 0,
            shared: Arc::new(Shared::new()),
            wake_alternate: Box::into_raw(Box::new(alternate)),
            results: VecDeque::new(),
        }
    }

    /// Test if the collection of futures is empty.
    pub fn is_empty(&self) -> bool {
        self.slab.is_empty()
    }

    /// Add the given future to the [Unordered] stream.
    ///
    /// Newly added futures are guaranteed to be polled, but there is no
    /// guarantee in which order this will happen.
    pub fn push(&mut self, future: F) {
        let index = self.slab.insert(future);
        self.max_index = usize::max(self.max_index, index);
        self.pollable.push(index);
    }
}

impl<F> Drop for Unordered<F>
where
    F: Future,
{
    fn drop(&mut self) {
        // Safety: we uniquely own `wake_alternate`, so we are responsible for
        // dropping it. This is asserted when we swap it out during a poll by
        // calling WakeSet::lock_write. We are also the _only_ one
        // swapping `wake_alternative`, so we know that can't happen here.
        unsafe {
            WakeSet::drop_raw(self.wake_alternate);
        }
    }
}

impl<F> Stream for Unordered<F>
where
    F: Future,
{
    type Item = F::Output;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Self {
            ref mut pollable,
            ref mut results,
            ref mut slab,
            ref shared,
            ref mut wake_alternate,
            max_index,
            ..
        } = *self.as_mut();

        // Return pending result.
        if let Some(value) = results.pop_front() {
            cx.waker().wake_by_ref();
            return Poll::Ready(Some(value));
        }

        if slab.is_empty() {
            // Nothing to poll, nothing to add. End the stream since we don't have work to do.
            return Poll::Ready(None);
        }

        // Note: We defer swapping the waker until we are here since we `wake_by_ref` when
        // reading results, and if we don't have any child tasks (slab is empty) no one would
        // benefit from an update anyways.
        if !shared.waker.is_woken_by(cx.waker()) {
            shared.waker.swap(cx.waker().clone());
        }

        let wake_last = {
            unsafe {
                {
                    let set = (**wake_alternate).as_local_mut();

                    if set.capacity() <= max_index {
                        set.reserve(max_index + 1);
                    }
                }

                // Unlock. At this position, if someone adds an element to the wake set they are
                // also bound to call wake, which will cause us to wake up.
                //
                // There is a race going on between locking and unlocking, and it's beneficial
                // for child tasks to observe the locked state of the wake set so they refetch
                // the other set instead of having to wait until another wakeup.
                (**wake_alternate).unlock_write();

                let next = mem::replace(wake_alternate, ptr::null_mut());
                *wake_alternate = shared.wake_set.swap(next);

                // Make sure no one else is using the alternate wake.
                //
                // Safety: We are the only one swapping wake_alternate, so at
                // this point we know that we have access to the most recent
                // active set. We _must_ call lock_write before we
                // can punt this into a mutable reference though, because at
                // this point inner futures will still have access to references
                // to it (under a lock!). We must wait for these to expire.
                (**wake_alternate).lock_write();
                (**wake_alternate).as_local_mut()
            }
        };

        let indexes = iter::from_fn(|| pollable.pop()).chain(wake_last.drain());

        for index in indexes {
            // NB: Since we defer pollables a little, a future might
            // have been polled and subsequently removed from the slab.
            // So we don't treat this as an error here.
            // If on the other hand it was removed _and_ re-added, we have
            // a case of a spurious poll. Luckily, that doesn't bother a
            // future much.
            let fut = match slab.get_pin_mut(index) {
                Some(fut) => fut,
                None => continue,
            };

            // Construct a new lightweight waker only capable of waking by
            // reference, with referential access to `shared`.
            let result = self::waker::poll_with_ref(shared, index, move |cx| fut.poll(cx));

            if let Poll::Ready(result) = result {
                results.push_back(result);
                let removed = slab.remove(index);
                debug_assert!(removed);
            }
        }

        // Return produced result.
        if let Some(value) = results.pop_front() {
            cx.waker().wake_by_ref();
            return Poll::Ready(Some(value));
        }

        Poll::Pending
    }
}
