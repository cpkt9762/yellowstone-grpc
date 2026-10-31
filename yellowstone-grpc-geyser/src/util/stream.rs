use {
    futures::{task::AtomicWaker, Stream},
    std::{
        fmt,
        marker::PhantomData,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        task::{Context, Poll},
    },
    thiserror::Error,
    tokio::sync::{
        mpsc::{UnboundedReceiver, UnboundedSender},
        Semaphore, TryAcquireError,
    },
};

/// Largest capacity accepted by [`load_aware_channel`].
pub const MAX_CAPACITY: usize = Semaphore::MAX_PERMITS;

/// Error returned by [`LoadAwareSender::send`] when the [`LoadAwareReceiver`] is dropped.
///
/// It hands the item that could not be sent back to the caller.
#[derive(Error)]
#[error("load aware channel is closed")]
pub struct SendError<T>(pub T);

impl<T> SendError<T> {
    /// Takes the item that could not be sent back out of the error.
    ///
    /// # Returns
    ///
    /// The item that was passed to [`LoadAwareSender::send`].
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendError").finish_non_exhaustive()
    }
}

/// Error returned by [`LoadAwareSender::try_send`].
///
/// Both variants hand the item that could not be sent back to the caller.
#[derive(Error)]
pub enum TrySendError<T> {
    /// There is not enough free weight for the item right now, or another sender is already
    /// waiting for capacity.
    #[error("load aware channel is full")]
    Full(T),
    /// The [`LoadAwareReceiver`] is dropped.
    #[error("load aware channel is closed")]
    Closed(T),
}

impl<T> TrySendError<T> {
    /// Takes the item that could not be sent back out of the error.
    ///
    /// # Returns
    ///
    /// The item that was passed to [`LoadAwareSender::try_send`].
    pub fn into_inner(self) -> T {
        match self {
            Self::Full(item) | Self::Closed(item) => item,
        }
    }
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("Full(..)"),
            Self::Closed(_) => f.write_str("Closed(..)"),
        }
    }
}

/// Assigns the abstract "weight" a [`load_aware_channel`] uses for backpressure to each item.
///
/// The weight is an opaque score: the channel never interprets it, it only sums the weights of
/// the items currently queued and compares that sum against the channel capacity. Every
/// [`LoadAwareSender`] owns a copy of the weigher, so a weigher can carry configuration such as
/// per-kind scales.
pub trait Weigher {
    /// The type of item this weigher weighs.
    type Item;

    /// Returns the weight of `item`.
    ///
    /// # Arguments
    ///
    /// * `item` - The item about to be queued.
    ///
    /// # Returns
    ///
    /// The weight of the item. A weight of `0` is treated as `1` by the channel, and a weight
    /// above the channel capacity is treated as the full capacity.
    fn weight(&self, item: &Self::Item) -> u32;
}

/// A [`Weigher`] that weighs every item `1`, so the channel capacity bounds the item count.
pub struct UnitWeigher<T>(PhantomData<fn(&T)>);

impl<T> UnitWeigher<T> {
    /// Creates a [`UnitWeigher`].
    ///
    /// # Returns
    ///
    /// A [`UnitWeigher`] for items of type `T`.
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> Default for UnitWeigher<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for UnitWeigher<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for UnitWeigher<T> {}

impl<T> fmt::Debug for UnitWeigher<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UnitWeigher")
    }
}

impl<T> Weigher for UnitWeigher<T> {
    type Item = T;

    fn weight(&self, _item: &T) -> u32 {
        1
    }
}

/// State shared between every [`LoadAwareSender`] and the [`LoadAwareReceiver`].
#[derive(Debug)]
struct Shared {
    cancelled: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    revision_credit_owners: std::sync::atomic::AtomicUsize,
    /// One permit per unit of weight. A sender acquires the item weight before queueing and the
    /// receiver gives it back on dequeue. Waiting senders are served in strict FIFO order.
    semaphore: Semaphore,
    /// Total weight the channel can hold.
    capacity: usize,
}

impl Shared {
    /// Computes the weight an item is accounted for while it sits in the channel.
    ///
    /// A weight of `0` is floored to `1` so that every queued item consumes capacity, and a
    /// weight above `capacity` is clamped to `capacity` so an oversized item is admitted once
    /// the channel is empty instead of waiting forever.
    ///
    /// # Arguments
    ///
    /// * `weight` - The weight of the item about to be queued.
    ///
    /// # Returns
    ///
    /// The admitted weight, in `1..=capacity`. It always fits in a `u32` because the weight
    /// does.
    fn admitted_weight(&self, weight: u32) -> u32 {
        (weight.max(1) as usize).min(self.capacity) as u32
    }
}

// Shared by all lanes and clones of this physical queue, unlike lane credits.
#[derive(Default)]
struct PublicationState {
    priority: AtomicBool,
    publishing: AtomicUsize,
    waker: AtomicWaker,
}
struct Publication<'a>(Option<&'a PublicationState>);
impl PublicationState {
    fn enter(&self) -> Publication<'_> {
        if self.priority.load(Ordering::SeqCst) {
            self.publishing.fetch_add(1, Ordering::SeqCst);
            Publication(Some(self))
        } else {
            Publication(None)
        }
    }
}
impl Drop for Publication<'_> {
    fn drop(&mut self) {
        if let Some(state) = self.0 {
            state.publishing.fetch_sub(1, Ordering::SeqCst);
            state.waker.wake();
        }
    }
}
// Primary credit uses only its weight, as in the original A queue. Only an
// independent B lane carries ownership so its pool survives sender teardown.
struct QueueCredit {
    weight: u32,
    independent: Option<Arc<Shared>>,
}
impl QueueCredit {
    fn release(self, primary: &Shared) {
        if self.independent.is_none() {
            primary.semaphore.add_permits(self.weight as usize);
        }
        // Drop returns independent credit to that lane, never to primary/A.
    }
}
impl Drop for QueueCredit {
    fn drop(&mut self) {
        if let Some(shared) = &self.independent {
            shared.semaphore.add_permits(self.weight as usize);
        }
    }
}

#[derive(Debug)]
pub struct DeferredCredit {
    owner: Option<Arc<Shared>>,
    weight: u32,
}
impl Drop for DeferredCredit {
    fn drop(&mut self) {
        if let Some(s) = self.owner.take() {
            s.semaphore.add_permits(self.weight as usize);
        }
    }
}

/// Sender end of the channel created by [`load_aware_channel`].
///
/// It can be cloned freely, every clone shares the same capacity and weighs items with its own
/// clone of the [`Weigher`] `W`.
#[cfg(test)]
#[derive(Debug)]
struct PublicationPause {
    after_send: bool,
    entered: std::sync::Barrier,
    resume: std::sync::Barrier,
}
pub struct LoadAwareSender<T, W> {
    publication: Arc<PublicationState>,
    shared: Arc<Shared>,
    inner: UnboundedSender<(QueueCredit, T)>,
    independent: bool,
    weigher: W,
    #[cfg(test)]
    publication_pause: Option<Arc<PublicationPause>>,
}

impl<T, W> fmt::Debug for LoadAwareSender<T, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadAwareSender")
            .field("shared", &self.shared)
            .finish_non_exhaustive()
    }
}

impl<T, W: Clone> Clone for LoadAwareSender<T, W> {
    fn clone(&self) -> Self {
        Self {
            publication: Arc::clone(&self.publication),
            shared: Arc::clone(&self.shared),
            inner: self.inner.clone(),
            independent: self.independent,
            weigher: self.weigher.clone(),
            #[cfg(test)]
            publication_pause: self.publication_pause.clone(),
        }
    }
}

/// Receiver end of the channel created by [`load_aware_channel`].
///
/// Dropping it closes the channel and wakes every sender waiting for capacity.
pub struct LoadAwareReceiver<T> {
    publication: Arc<PublicationState>,
    shared: Arc<Shared>,
    inner: UnboundedReceiver<(QueueCredit, T)>,
    deferred: std::collections::VecDeque<(QueueCredit, T)>,
    #[cfg(test)]
    pending_injection: Option<Box<dyn FnOnce() + Send + Sync>>,
}

// No field is structurally pinned; moving queued values is the receiver API.
impl<T> Unpin for LoadAwareReceiver<T> {}
impl<T> Drop for LoadAwareReceiver<T> {
    fn drop(&mut self) {
        self.shared.semaphore.close();
    }
}

/// Creates an mpsc channel whose capacity is a total weight instead of an item count.
///
/// Each item is accounted for by the [`Weigher::weight`] of `weigher` from the moment a sender admits it
/// until the receiver dequeues it. The uncontended path is a single atomic operation plus a
/// lock-free enqueue, and a lock is only taken when a sender actually has to wait.
///
/// Semantics:
///
/// * Senders waiting for capacity are served in strict FIFO order, so a heavy item at the head
///   of the line blocks the lighter ones behind it even if they would fit.
/// * [`LoadAwareSender::try_send`] never jumps ahead of a waiting [`LoadAwareSender::send`].
/// * A weight of `0` counts as `1`, and a weight above `capacity` counts as `capacity`, meaning
///   such an item is only admitted into an empty channel.
///
/// # Arguments
///
/// * `weighted_capacity` - The total weight the channel can hold.
/// * `weigher` - The [`Weigher`] that weighs each item as a sender queues it.
///
/// # Returns
///
/// The [`LoadAwareSender`] and [`LoadAwareReceiver`] halves of the channel.
///
/// # Panics
///
/// Panics if `weighted_capacity` is `0` or exceeds [`MAX_CAPACITY`].
pub fn load_aware_channel<T, W>(
    weighted_capacity: usize,
    weigher: W,
) -> (LoadAwareSender<T, W>, LoadAwareReceiver<T>)
where
    W: Weigher<Item = T>,
{
    assert!(
        weighted_capacity > 0,
        "load aware channel weight capacity must be positive"
    );
    let (inner_sender, inner_receiver) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        cancelled: Default::default(),
        #[cfg(test)]
        revision_credit_owners: Default::default(),
        semaphore: Semaphore::new(weighted_capacity),
        capacity: weighted_capacity,
    });
    let publication = Arc::new(PublicationState::default());
    let sender = LoadAwareSender {
        publication: Arc::clone(&publication),
        shared: Arc::clone(&shared),
        inner: inner_sender,
        independent: false,
        weigher,
        #[cfg(test)]
        publication_pause: None,
    };

    let rx = LoadAwareReceiver {
        publication,
        shared,
        inner: inner_receiver,
        deferred: std::collections::VecDeque::new(),
        #[cfg(test)]
        pending_injection: None,
    };

    (sender, rx)
}

impl<T, W> LoadAwareSender<T, W>
where
    W: Weigher<Item = T>,
{
    pub fn cancel_independent(&self) {
        if self.independent {
            self.shared
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
    pub fn prioritize_primary(&self) {
        self.publication.priority.store(true, Ordering::SeqCst);
    }
    #[cfg(test)]
    fn pause_publication(&self, after_send: bool) {
        if let Some(pause) = &self.publication_pause {
            if pause.after_send == after_send {
                pause.entered.wait();
                pause.resume.wait();
            }
        }
    }
    pub fn reserve_independent(&self, weight: usize) -> Result<DeferredCredit, &'static str> {
        if !self.independent || weight > self.shared.capacity {
            return Err("invalid deferred B credit");
        }
        let weight = u32::try_from(weight).map_err(|_| "deferred B weight overflow")?;
        let permit = self
            .shared
            .semaphore
            .try_acquire_many(weight)
            .map_err(|_| "shared B queue credit exhausted")?;
        permit.forget();
        Ok(DeferredCredit {
            owner: Some(Arc::clone(&self.shared)),
            weight,
        })
    }
    pub fn try_send_prepaid(
        &self,
        item: T,
        mut credit: DeferredCredit,
    ) -> Result<(), TrySendError<T>> {
        if !self.independent
            || !credit
                .owner
                .as_ref()
                .is_some_and(|s| Arc::ptr_eq(s, &self.shared))
            || credit.weight < self.weigher.weight(&item)
        {
            return Err(TrySendError::Full(item));
        }
        let queued = QueueCredit {
            weight: credit.weight,
            independent: credit.owner.take(),
        };
        let _publication = self.publication.enter();
        #[cfg(test)]
        self.pause_publication(false);
        let result = self.inner.send((queued, item)).map_err(|e| {
            let (credit, item) = e.0;
            credit.release(&self.shared);
            TrySendError::Closed(item)
        });
        #[cfg(test)]
        self.pause_publication(true);
        result
    }
    fn queue_credit(&self, weight: u32) -> QueueCredit {
        let independent = if self.independent {
            #[cfg(test)]
            self.shared
                .revision_credit_owners
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(Arc::clone(&self.shared))
        } else {
            None
        };
        QueueCredit {
            weight,
            independent,
        }
    }
    /// Returns the weight `item` is accounted for while it sits in the channel.
    fn admitted_weight(&self, item: &T) -> u32 {
        self.shared.admitted_weight(self.weigher.weight(item))
    }

    /// Sends an item, waiting until the channel has enough free weight for it and every sender
    /// ahead of it in line.
    ///
    /// Cancel safe: dropping the returned future gives back any capacity it already reserved.
    ///
    /// # Arguments
    ///
    /// * `item` - The item to queue, weighed by the sender's [`Weigher`].
    ///
    /// # Returns
    ///
    /// `Ok(())` once the item is queued.
    ///
    /// # Errors
    ///
    /// Returns the `item` back in a [`SendError`] if the [`LoadAwareReceiver`] is dropped.
    pub async fn send(&self, item: T) -> Result<(), SendError<T>> {
        let weight = self.admitted_weight(&item);
        if self.independent && self.inner.is_closed() {
            return Err(SendError(item));
        }
        // Primary admission retains the baseline borrowed-permit path. B waits
        // also observe mpsc closure because only the primary semaphore is closed.
        let permit = match self.shared.semaphore.try_acquire_many(weight) {
            Ok(permit) => permit,
            Err(TryAcquireError::Closed) => return Err(SendError(item)),
            Err(TryAcquireError::NoPermits) => {
                let result = if self.independent {
                    tokio::select! {
                        _ = self.inner.closed() => return Err(SendError(item)),
                        result = self.shared.semaphore.acquire_many(weight) => result,
                    }
                } else {
                    self.shared.semaphore.acquire_many(weight).await
                };
                match result {
                    Ok(permit) => permit,
                    Err(_) => return Err(SendError(item)),
                }
            }
        };
        let credit = self.queue_credit(weight);
        permit.forget();
        let _publication = self.publication.enter();
        #[cfg(test)]
        self.pause_publication(false);
        let result = self.inner.send((credit, item)).map_err(|err| {
            let (credit, item) = err.0;
            credit.release(&self.shared);
            SendError(item)
        });
        #[cfg(test)]
        self.pause_publication(true);
        result
    }

    /// Independent credit pool, sharing exactly the same ordered mpsc queue.
    /// No legacy capacity is consumed. Oversized items are explicitly rejected.
    pub fn with_budget(&self, capacity: usize) -> Result<Self, &'static str>
    where
        W: Clone,
    {
        if capacity == 0 || capacity > MAX_CAPACITY {
            return Err("invalid queue credit capacity");
        }
        Ok(Self {
            publication: Arc::clone(&self.publication),
            shared: Arc::new(Shared {
                cancelled: Default::default(),
                #[cfg(test)]
                revision_credit_owners: Default::default(),
                semaphore: Semaphore::new(capacity),
                capacity,
            }),
            inner: self.inner.clone(),
            independent: true,
            weigher: self.weigher.clone(),
            #[cfg(test)]
            publication_pause: self.publication_pause.clone(),
        })
    }

    /// Sends without waiting; preserves FIFO and releases the correct lane's permits.
    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        if self.independent && self.inner.is_closed() {
            return Err(TrySendError::Closed(item));
        }
        let weight = self.admitted_weight(&item);
        let permit = match self.shared.semaphore.try_acquire_many(weight) {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => return Err(TrySendError::Full(item)),
            Err(TryAcquireError::Closed) => return Err(TrySendError::Closed(item)),
        };
        let credit = self.queue_credit(weight);
        permit.forget();
        let _publication = self.publication.enter();
        #[cfg(test)]
        self.pause_publication(false);
        let result = self.inner.send((credit, item)).map_err(|err| {
            let (credit, item) = err.0;
            credit.release(&self.shared);
            TrySendError::Closed(item)
        });
        #[cfg(test)]
        self.pause_publication(true);
        result
    }

    /// Strict admission for B: never clamp a message larger than its byte budget.
    pub fn try_send_strict(&self, item: T) -> Result<(), TrySendError<T>> {
        if self.weigher.weight(&item) as usize > self.shared.capacity {
            return Err(TrySendError::Full(item));
        }
        self.try_send(item)
    }

    /// Returns the weight currently held by the channel.
    ///
    /// This includes capacity partially reserved by senders waiting for room, so it can be
    /// higher than the sum of the weights of the queued items while a sender is waiting.
    ///
    /// # Returns
    ///
    /// The held weight, in `0..=capacity`.
    pub fn current_weight(&self) -> usize {
        self.shared.capacity - self.shared.semaphore.available_permits()
    }
}

impl<T> LoadAwareReceiver<T> {
    #[cfg(test)]
    pub(crate) fn revision_credit_owners(&self) -> usize {
        self.shared
            .revision_credit_owners
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Receives the next item, releasing its weight back to the channel.
    ///
    /// # Returns
    ///
    /// The next item, or [`None`] once the channel is empty and every [`LoadAwareSender`] is
    /// dropped.
    pub async fn recv(&mut self) -> Option<T> {
        use std::future::poll_fn;
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    fn poll_unconstrained(&mut self, cx: &mut Context<'_>) -> Poll<Option<(QueueCredit, T)>> {
        use std::future::{poll_fn, Future};
        std::pin::pin!(tokio::task::coop::unconstrained(poll_fn(|cx| self
            .inner
            .poll_recv(cx))))
        .poll(cx)
    }
    /// Polls for the next item, releasing its weight back to the channel.
    ///
    /// # Arguments
    ///
    /// * `cx` - The task [`Context`] to register for wake-up.
    ///
    /// # Returns
    ///
    /// [`Poll::Ready`] with the next item, or with [`None`] once the channel is empty and every
    /// [`LoadAwareSender`] is dropped. [`Poll::Pending`] otherwise.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if self.publication.priority.load(Ordering::SeqCst) {
            let mut confirmed_empty = false;
            let mut closed = false;
            // len may exclude a reserved unpublished tail. It is used only as
            // a finite work bound; one unit always permits the recheck protocol.
            let limit = self.inner.len().saturating_add(1);
            for _ in 0..limit {
                let item = match self.poll_unconstrained(cx) {
                    Poll::Ready(item) => item,
                    Poll::Pending => {
                        #[cfg(test)]
                        if let Some(inject) = self.pending_injection.take() {
                            inject();
                        }
                        self.publication.waker.register(cx.waker());
                        if self.publication.publishing.load(Ordering::SeqCst) != 0 {
                            return Poll::Pending;
                        }
                        // A publisher may have completed between the first poll
                        // and this zero observation. Re-poll before sending B.
                        match self.poll_unconstrained(cx) {
                            Poll::Ready(item) => item,
                            Poll::Pending => {
                                confirmed_empty = true;
                                break;
                            }
                        }
                    }
                };
                let Some((credit, item)) = item else {
                    confirmed_empty = true;
                    closed = true;
                    break;
                };
                if credit.independent.is_none() {
                    credit.release(&self.shared);
                    return Poll::Ready(Some(item));
                }
                if credit
                    .independent
                    .as_ref()
                    .is_some_and(|s| s.cancelled.load(Ordering::Acquire))
                {
                    credit.release(&self.shared);
                    drop(item);
                } else {
                    self.deferred.push_back((credit, item));
                }
            }
            if !confirmed_empty {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            while let Some((credit, item)) = self.deferred.pop_front() {
                let cancelled = credit
                    .independent
                    .as_ref()
                    .is_some_and(|s| s.cancelled.load(Ordering::Acquire));
                credit.release(&self.shared);
                if !cancelled {
                    return Poll::Ready(Some(item));
                }
            }
            return if closed {
                Poll::Ready(None)
            } else {
                Poll::Pending
            };
        }
        self.inner.poll_recv(cx).map(|maybe| {
            maybe.map(|(credit, item)| {
                credit.release(&self.shared);
                item
            })
        })
    }
}

impl<T> Stream for LoadAwareReceiver<T> {
    type Item = T;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        futures::poll,
        std::{pin::pin, task::Poll},
        tokio::task::yield_now,
        tokio_stream::StreamExt,
    };

    #[derive(Debug, PartialEq, Eq)]
    struct TestItem(u32);

    /// Weighs a [`TestItem`] by the value it holds.
    #[derive(Debug, Clone, Copy)]
    struct TestWeigher;

    impl Weigher for TestWeigher {
        type Item = TestItem;

        fn weight(&self, item: &TestItem) -> u32 {
            item.0
        }
    }

    /// Weighs a [`TestItem`] by the value it holds times a configured factor.
    #[derive(Debug, Clone, Copy)]
    struct ScaledWeigher(u32);

    impl Weigher for ScaledWeigher {
        type Item = TestItem;

        fn weight(&self, item: &TestItem) -> u32 {
            item.0 * self.0
        }
    }

    #[tokio::test]
    async fn revision2_n2_independent_wait_cancel_and_close_return_credit() {
        use futures::poll;
        use std::pin::pin;
        let (a, rx) = load_aware_channel(1, TestWeigher);
        let b = a.with_budget(4).expect("independent lane");
        b.send(TestItem(3)).await.expect("B send");
        {
            let mut waiting = pin!(b.send(TestItem(3)));
            assert!(poll!(&mut waiting).is_pending());
            assert_eq!(b.current_weight(), 4);
        }
        assert_eq!(
            b.current_weight(),
            3,
            "cancel returns partial borrowed reservation"
        );
        let mut waiting = pin!(b.send(TestItem(3)));
        assert!(poll!(&mut waiting).is_pending());
        drop(rx);
        assert!(
            matches!(poll!(&mut waiting), Poll::Ready(Err(_))),
            "B wait observes mpsc closure"
        );
        assert_eq!(
            b.current_weight(),
            0,
            "dropped queue and cancelled wait release B credit"
        );
        assert!(b.send(TestItem(1)).await.is_err());
        assert!(matches!(
            b.try_send(TestItem(1)),
            Err(TrySendError::Closed(_))
        ));
        assert_eq!(a.current_weight(), 0, "B never releases credit into A");
    }
    #[tokio::test]
    async fn revision2_n2_primary_credit_has_no_per_message_ownership_clones() {
        let (sender, mut receiver) = load_aware_channel(4, TestWeigher);
        sender.try_send(TestItem(2)).expect("primary try_send");
        sender.send(TestItem(2)).await.expect("primary send");
        {
            let mut waiting = pin!(sender.send(TestItem(1)));
            assert!(poll!(&mut waiting).is_pending());
        }
        assert_eq!(receiver.recv().await, Some(TestItem(2)));
        assert_eq!(receiver.recv().await, Some(TestItem(2)));
        assert_eq!(
            sender.current_weight(),
            0,
            "cancel and dequeue return all credit"
        );
        sender.try_send(TestItem(4)).expect("fill");
        let mut waiting = pin!(sender.send(TestItem(3)));
        assert!(poll!(&mut waiting).is_pending());
        assert_eq!(receiver.recv().await, Some(TestItem(4)));
        assert!(matches!(poll!(&mut waiting), Poll::Ready(Ok(()))));
        assert_eq!(receiver.recv().await, Some(TestItem(3)));
        assert_eq!(
            receiver.revision_credit_owners(),
            0,
            "zero B queue admission/wait/cancel/dequeue must not clone credit ownership"
        );
    }
    #[tokio::test]
    async fn test_basic_send_and_receive() {
        let (sender, mut receiver) = load_aware_channel(10, TestWeigher);

        sender.send(TestItem(5)).await.unwrap();
        assert_eq!(sender.current_weight(), 5);
        let received = receiver.recv().await.unwrap();
        assert_eq!(sender.current_weight(), 0);
        assert_eq!(received.0, 5);
    }

    #[tokio::test]
    async fn test_stream_behavior() {
        let (sender, receiver) = load_aware_channel(10, TestWeigher);

        sender.send(TestItem(1)).await.unwrap();
        sender.send(TestItem(2)).await.unwrap();
        sender.send(TestItem(3)).await.unwrap();

        assert_eq!(sender.current_weight(), 6);
        drop(sender);
        let mut stream = receiver;

        let mut results = vec![];
        while let Some(item) = stream.next().await {
            results.push(item.0);
        }

        assert_eq!(results, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn waiting_senders_are_served_in_fifo_order() {
        let (sender, mut receiver) = load_aware_channel(10, TestWeigher);
        sender.send(TestItem(3)).await.unwrap();
        sender.send(TestItem(3)).await.unwrap();

        // Heavy item reserves the 4 free units, then waits for 4 more.
        let mut heavy = pin!(sender.send(TestItem(8)));
        assert!(poll!(&mut heavy).is_pending());
        // The light item would fit once a single light item leaves, but it is behind the heavy one.
        let mut light = pin!(sender.send(TestItem(1)));
        assert!(poll!(&mut light).is_pending());

        // Frees 3 units: the heavy item still needs 1, so the light item must keep waiting.
        assert_eq!(receiver.recv().await, Some(TestItem(3)));
        assert!(poll!(&mut heavy).is_pending());
        assert!(poll!(&mut light).is_pending());

        // Frees 3 more: the heavy item completes, then the light one gets the leftover.
        assert_eq!(receiver.recv().await, Some(TestItem(3)));
        assert!(matches!(poll!(&mut heavy), Poll::Ready(Ok(()))));
        assert!(matches!(poll!(&mut light), Poll::Ready(Ok(()))));

        assert_eq!(receiver.recv().await, Some(TestItem(8)));
        assert_eq!(receiver.recv().await, Some(TestItem(1)));
    }

    #[tokio::test]
    async fn try_send_does_not_jump_ahead_of_a_waiting_send() {
        let (sender, mut receiver) = load_aware_channel(10, TestWeigher);
        sender.send(TestItem(6)).await.unwrap();

        let mut heavy = pin!(sender.send(TestItem(8)));
        assert!(poll!(&mut heavy).is_pending());

        // 4 units are nominally free, but the waiting sender is first in line.
        assert!(matches!(
            sender.try_send(TestItem(1)),
            Err(TrySendError::Full(TestItem(1)))
        ));

        assert_eq!(receiver.recv().await, Some(TestItem(6)));
        assert!(matches!(poll!(&mut heavy), Poll::Ready(Ok(()))));
        // Nothing is waiting anymore, so try_send admits what fits.
        sender.try_send(TestItem(2)).unwrap();
    }

    #[tokio::test]
    async fn dropping_a_waiting_send_gives_its_capacity_back() {
        let (sender, mut receiver) = load_aware_channel(10, TestWeigher);
        sender.send(TestItem(6)).await.unwrap();

        {
            let mut heavy = pin!(sender.send(TestItem(8)));
            assert!(poll!(&mut heavy).is_pending());
            // It holds the 4 free units while it waits.
            assert_eq!(sender.current_weight(), 10);
        }

        assert_eq!(sender.current_weight(), 6);
        sender.try_send(TestItem(4)).unwrap();
        assert_eq!(receiver.recv().await, Some(TestItem(6)));
        assert_eq!(receiver.recv().await, Some(TestItem(4)));
        assert_eq!(sender.current_weight(), 0);
    }

    #[tokio::test]
    async fn dropping_the_receiver_wakes_waiting_senders() {
        let (sender, receiver) = load_aware_channel(2, TestWeigher);
        sender.send(TestItem(2)).await.unwrap();

        let mut waiting = pin!(sender.send(TestItem(1)));
        assert!(poll!(&mut waiting).is_pending());

        drop(receiver);

        match poll!(&mut waiting) {
            Poll::Ready(Err(SendError(item))) => assert_eq!(item, TestItem(1)),
            other => panic!("expected the waiting send to fail, got {other:?}"),
        }
        assert!(matches!(
            sender.try_send(TestItem(1)),
            Err(TrySendError::Closed(TestItem(1)))
        ));
        assert!(sender.send(TestItem(1)).await.is_err());
    }

    #[tokio::test]
    async fn oversized_item_is_admitted_only_into_an_empty_channel() {
        let (sender, mut receiver) = load_aware_channel(10, TestWeigher);

        // Empty channel: admitted immediately and accounted as the full capacity.
        sender.send(TestItem(100)).await.unwrap();
        assert_eq!(sender.current_weight(), 10);
        assert!(matches!(
            sender.try_send(TestItem(1)),
            Err(TrySendError::Full(_))
        ));
        assert_eq!(receiver.recv().await, Some(TestItem(100)));
        assert_eq!(sender.current_weight(), 0);

        // Non-empty channel: has to wait for it to drain.
        sender.send(TestItem(3)).await.unwrap();
        let mut oversized = pin!(sender.send(TestItem(100)));
        assert!(poll!(&mut oversized).is_pending());
        assert_eq!(receiver.recv().await, Some(TestItem(3)));
        assert!(matches!(poll!(&mut oversized), Poll::Ready(Ok(()))));
        assert_eq!(receiver.recv().await, Some(TestItem(100)));
        assert_eq!(sender.current_weight(), 0);
    }

    #[tokio::test]
    async fn weigher_state_sets_the_weight() {
        let (sender, mut receiver) = load_aware_channel(10, ScaledWeigher(3));
        let clone = sender.clone();

        sender.try_send(TestItem(2)).unwrap();
        assert_eq!(sender.current_weight(), 6);
        // A cloned sender weighs with the same scale.
        assert!(matches!(
            clone.try_send(TestItem(2)),
            Err(TrySendError::Full(TestItem(2)))
        ));
        assert_eq!(receiver.recv().await, Some(TestItem(2)));
        assert_eq!(sender.current_weight(), 0);
    }

    #[tokio::test]
    async fn zero_weight_items_still_consume_capacity() {
        let (sender, mut receiver) = load_aware_channel(2, TestWeigher);
        sender.try_send(TestItem(0)).unwrap();
        sender.try_send(TestItem(0)).unwrap();
        assert_eq!(sender.current_weight(), 2);
        assert!(matches!(
            sender.try_send(TestItem(0)),
            Err(TrySendError::Full(_))
        ));
        receiver.recv().await.unwrap();
        receiver.recv().await.unwrap();
        assert_eq!(sender.current_weight(), 0);
    }

    #[derive(Debug)]
    struct StressItem {
        sender_id: usize,
        seq: usize,
        weight: u32,
    }

    /// Weighs a [`StressItem`] by its `weight` field.
    #[derive(Debug, Clone, Copy)]
    struct StressWeigher;

    impl Weigher for StressWeigher {
        type Item = StressItem;

        fn weight(&self, item: &StressItem) -> u32 {
            item.weight
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_senders_deliver_everything_and_release_all_weight() {
        const CAPACITY: usize = 20;
        const SENDERS: usize = 4;
        const ITEMS_PER_SENDER: usize = 5_000;

        let (sender, mut receiver) = load_aware_channel(CAPACITY, StressWeigher);
        let probe = sender.clone();

        let mut tasks = Vec::new();
        for sender_id in 0..SENDERS {
            let sender = sender.clone();
            tasks.push(tokio::spawn(async move {
                for seq in 0..ITEMS_PER_SENDER {
                    let item = StressItem {
                        sender_id,
                        seq,
                        weight: ((seq + sender_id) % 7 + 1) as u32,
                    };
                    // Odd senders spin on try_send, even ones use the waiting send.
                    if sender_id % 2 == 1 {
                        let mut item = item;
                        loop {
                            match sender.try_send(item) {
                                Ok(()) => break,
                                Err(TrySendError::Full(back)) => {
                                    item = back;
                                    yield_now().await;
                                }
                                Err(TrySendError::Closed(_)) => panic!("receiver dropped"),
                            }
                        }
                    } else {
                        sender.send(item).await.unwrap();
                    }
                }
            }));
        }
        drop(sender);

        let mut next_seq = [0usize; SENDERS];
        for _ in 0..SENDERS * ITEMS_PER_SENDER {
            let item = receiver.recv().await.expect("channel closed early");
            assert_eq!(
                item.seq, next_seq[item.sender_id],
                "per sender order broken"
            );
            next_seq[item.sender_id] += 1;
            assert!(probe.current_weight() <= CAPACITY);
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(next_seq, [ITEMS_PER_SENDER; SENDERS]);
        assert_eq!(probe.current_weight(), 0);
    }
}

#[cfg(test)]
mod order_writes_v1 {
    use super::*;
    #[tokio::test]
    async fn revision2_n2_queued_independent_credit_outlives_sender() {
        let (a, mut rx) = load_aware_channel(1, UnitWeigher::<u32>::new());
        let b = a.with_budget(1).expect("independent lane");
        let credit = Arc::downgrade(&b.shared);
        b.send(7).await.expect("B queued");
        assert_eq!(
            b.shared
                .revision_credit_owners
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "audit counts the actual independent ownership clone"
        );
        drop(b);
        assert!(
            credit.upgrade().is_some(),
            "queued B credit retains its own lane"
        );
        a.try_send(8).expect("independent A credit");
        assert_eq!(rx.recv().await, Some(7));
        assert!(credit.upgrade().is_none(), "dequeue releases last B owner");
        assert_eq!(a.current_weight(), 1, "B dequeue cannot release A credit");
        assert_eq!(rx.recv().await, Some(8));
        assert_eq!(a.current_weight(), 0);
    }
    #[tokio::test]
    async fn b_full_and_reserved_full_preserve_a_capacity_and_fifo() {
        let (a, mut rx) = load_aware_channel(1, UnitWeigher::<u32>::new());
        let b = a.with_budget(1).expect("B credit");
        let control = a.with_budget(1).expect("control credit");
        b.try_send(1).expect("B event");
        assert!(matches!(b.try_send(2), Err(TrySendError::Full(2))));
        control.try_send(3).expect("gap reserve");
        assert!(matches!(control.try_send(4), Err(TrySendError::Full(4))));
        a.try_send(5)
            .expect("A capacity remains independently available");
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(3));
        assert_eq!(rx.recv().await, Some(5));
        assert_eq!(a.current_weight(), 0);
        assert_eq!(b.current_weight(), 0);
        assert_eq!(control.current_weight(), 0);
        drop(rx);
        assert!(matches!(b.try_send(6), Err(TrySendError::Closed(6))));
    }
}

#[cfg(test)]
mod tracked_account_history_v1 {
    use super::*;
    struct WakeProbe(std::sync::atomic::AtomicUsize);
    impl futures::task::ArcWake for WakeProbe {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    async fn controlled_publication(after_send: bool, method: u8) {
        let (a, mut rx) = load_aware_channel(1, UnitWeigher::<u32>::new());
        let b = a.with_budget(2).expect("B pool");
        a.prioritize_primary();
        b.try_send(0).expect("old deferred B");
        let item = tokio::task::coop::unconstrained(rx.inner.recv())
            .await
            .expect("move B to deferred without releasing credit");
        rx.deferred.push_back(item);
        let pause = Arc::new(PublicationPause {
            after_send,
            entered: std::sync::Barrier::new(2),
            resume: std::sync::Barrier::new(2),
        });
        let is_a = !after_send && method != 2;
        let mut sender = if is_a { a.clone() } else { b.clone() };
        sender.publication_pause = Some(Arc::clone(&pause));
        let probe = Arc::new(WakeProbe(std::sync::atomic::AtomicUsize::new(0)));
        let waker = futures::task::waker(Arc::clone(&probe));
        let mut cx = Context::from_waker(&waker);
        let pending = std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                if method == 0 {
                    sender.try_send(1).expect("controlled synchronous send");
                } else if method == 1 {
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .expect("sender runtime")
                        .block_on(sender.send(1))
                        .expect("controlled async send");
                } else {
                    let credit = sender.reserve_independent(1).expect("prepaid B credit");
                    sender
                        .try_send_prepaid(1, credit)
                        .expect("controlled prepaid send");
                }
            });
            pause.entered.wait();
            let pending = matches!(rx.poll_recv(&mut cx), Poll::Pending);
            pause.resume.wait();
            writer.join().expect("publisher resumed");
            pending
        });
        assert!(
            pending,
            "incomplete publication must yield, never send deferred B"
        );
        assert!(
            probe.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "publication exit wakes receiver"
        );
        if is_a {
            assert_eq!(rx.recv().await, Some(1), "published A first");
            assert_eq!(a.current_weight(), 0);
        }
        assert_eq!(rx.recv().await, Some(0), "deferred B-only progress");
        if !is_a {
            assert_eq!(rx.recv().await, Some(1), "B publication preserves FIFO");
        }
        assert_eq!(b.current_weight(), 0);
        drop((a, b, sender));
        assert_eq!(rx.recv().await, None, "closure after draining");
    }
    async fn transition_sender_clone(payload:Vec<u8>, asynchronous:bool) {
        let (a,mut rx)=load_aware_channel(1,UnitWeigher::<Vec<u8>>::new());
        let b=a.with_budget(1).expect("B lane shares publication state before enable");
        let pause=Arc::new(PublicationPause {after_send:false,entered:std::sync::Barrier::new(2),resume:std::sync::Barrier::new(2)});
        let mut independent_task_sender=a.clone();independent_task_sender.publication_pause=Some(Arc::clone(&pause));
        let probe=Arc::new(WakeProbe(AtomicUsize::new(0)));let waker=futures::task::waker(Arc::clone(&probe));let mut cx=Context::from_waker(&waker);
        let pending=std::thread::scope(|scope| {
            let writer=scope.spawn(|| {
                if asynchronous {
                    tokio::runtime::Builder::new_current_thread().build().expect("ping task runtime").block_on(independent_task_sender.send(payload.clone())).expect("ping sender clone");
                } else {independent_task_sender.try_send(payload.clone()).expect("incoming pong sender clone");}
            });
            pause.entered.wait();
            let was_disabled=!a.publication.priority.load(Ordering::SeqCst);
            a.prioritize_primary();
            b.try_send(vec![0x7f]).expect("B produced after enabling priority");
            let queued=rx.poll_unconstrained(&mut cx);
            let deferred=if let Poll::Ready(Some(item))=queued {rx.deferred.push_back(item);true}else{false};
            let pending=if deferred {rx.poll_recv(&mut cx).is_pending()}else{false};
            pause.resume.wait();writer.join().expect("old sender exits");
            (was_disabled,deferred,pending)
        });
        assert!(pending.0,"sender entered while priority was false");assert!(pending.1,"B already deferred");
        assert!(pending.2,"old in-flight sender must prevent B passing the activation boundary");
        assert!(probe.0.load(Ordering::SeqCst)>0,"old sender exit wakes priority receiver");
        assert_eq!(rx.recv().await,Some(payload),"ping/pong bytes precede deferred B unchanged");
        assert_eq!(a.current_weight(),0,"original A permit released");
        assert_eq!(rx.recv().await,Some(vec![0x7f]));assert_eq!(b.current_weight(),0);
        assert_eq!(a.publication.publishing.load(Ordering::SeqCst),0);
        drop((a,b,independent_task_sender));assert_eq!(rx.recv().await,None);
    }
    #[tokio::test]
    async fn r4_activation_counts_old_ping_task_sender_clone() {
        use prost::Message;
        use yellowstone_grpc_proto::prelude::*;
        let raw=SubscribeUpdate {update_oneof:Some(subscribe_update::UpdateOneof::Ping(SubscribeUpdatePing {})),..Default::default()}.encode_to_vec();
        transition_sender_clone(raw,true).await;
    }
    #[tokio::test]
    async fn r4_activation_counts_old_incoming_pong_sender_clone() {
        use prost::Message;
        use yellowstone_grpc_proto::prelude::*;
        let raw=SubscribeUpdate {update_oneof:Some(subscribe_update::UpdateOneof::Pong(SubscribeUpdatePong {id:777})),..Default::default()}.encode_to_vec();
        transition_sender_clone(raw,false).await;
    }
    #[test]
    fn r4_unpaired_publication_counts_without_accessing_waker() {
        let state=PublicationState::default();
        let probe=Arc::new(WakeProbe(AtomicUsize::new(0)));let waker=futures::task::waker(Arc::clone(&probe));state.waker.register(&waker);
        let guard=state.enter();assert_eq!(state.publishing.load(Ordering::SeqCst),1,"unpaired sends are counted before inner.send too");
        drop(guard);assert_eq!(state.publishing.load(Ordering::SeqCst),0);
        assert_eq!(probe.0.load(Ordering::SeqCst),0,"disabled exit never wakes or consumes stored waker");
        state.waker.wake();assert_eq!(probe.0.load(Ordering::SeqCst),1,"waker remains registered after disabled exit");
    }
    #[tokio::test]
    async fn r3_registered_unpublished_a_defers_b_and_wakes() {
        controlled_publication(false, 0).await;
    }
    #[tokio::test]
    async fn r3_published_b_tail_defers_b_and_wakes() {
        controlled_publication(true, 1).await;
    }
    #[tokio::test]
    async fn r3_prepaid_published_tail_defers_b_and_wakes() {
        controlled_publication(true, 2).await;
    }
    #[tokio::test]
    async fn r3_unpublished_prepaid_b_defers_b_and_wakes() {
        controlled_publication(false, 2).await;
    }
    #[tokio::test]
    async fn r3_available_a_precedes_b_during_other_lane_publication() {
        let (a, mut rx) = load_aware_channel(1, UnitWeigher::<u32>::new());
        let b = a.with_budget(2).expect("B pool");
        a.prioritize_primary();
        b.try_send(0).expect("old B");
        let item = tokio::task::coop::unconstrained(rx.inner.recv())
            .await
            .expect("deferred B");
        rx.deferred.push_back(item);
        let pause = Arc::new(PublicationPause {
            after_send: true,
            entered: std::sync::Barrier::new(2),
            resume: std::sync::Barrier::new(2),
        });
        let mut sender = b.clone();
        sender.publication_pause = Some(Arc::clone(&pause));
        let probe = Arc::new(WakeProbe(std::sync::atomic::AtomicUsize::new(0)));
        let waker = futures::task::waker(Arc::clone(&probe));
        let mut cx = Context::from_waker(&waker);
        let observed = std::thread::scope(|scope| {
            let writer = scope.spawn(|| sender.try_send(1).expect("publishing B"));
            pause.entered.wait();
            a.try_send(99).expect("A behind just-published B");
            let first = rx.poll_recv(&mut cx);
            let tail = rx.poll_recv(&mut cx);
            pause.resume.wait();
            writer.join().expect("writer");
            (first, tail)
        });
        assert!(
            matches!(observed.0, Poll::Ready(Some(99))),
            "available A must not wait for the B publisher's tail"
        );
        assert!(
            observed.1.is_pending(),
            "deferred B waits for the publisher exit"
        );
        assert!(probe.0.load(std::sync::atomic::Ordering::SeqCst) > 0);
        assert_eq!(rx.recv().await, Some(0));
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!((a.current_weight(), b.current_weight()), (0, 0));
    }
    #[tokio::test]
    async fn r3_zero_publishing_requires_second_poll_before_deferred_b() {
        let (a, mut rx) = load_aware_channel(1, UnitWeigher::<u32>::new());
        let b = a.with_budget(1).expect("B pool");
        a.prioritize_primary();
        b.try_send(0).expect("B");
        let item = tokio::task::coop::unconstrained(rx.inner.recv())
            .await
            .expect("deferred B");
        rx.deferred.push_back(item);
        let publisher = a.clone();
        rx.pending_injection = Some(Box::new(move || {
            publisher
                .try_send(99)
                .expect("A completes between first Pending and counter load")
        }));
        assert_eq!(
            rx.recv().await,
            Some(99),
            "second poll must see the completed publisher"
        );
        assert_eq!(rx.recv().await, Some(0));
        assert_eq!((a.current_weight(), b.current_weight()), (0, 0));
    }
    #[tokio::test]
    async fn r2_a_priority_crosses_coop_budget_and_releases_a_credit() {
        let (a, mut rx) = load_aware_channel(1, UnitWeigher::<u32>::new());
        let b = a.with_budget(129).expect("bounded B backlog");
        a.prioritize_primary();
        for value in 0..129 {
            b.try_send(value).expect("B backlog");
        }
        a.try_send(999).expect("single original A permit");
        let first = tokio::spawn(async move {
            let first = rx.recv().await;
            (first, rx)
        })
        .await
        .expect("fresh Tokio coop budget");
        let (first, mut rx) = first;
        assert_eq!(
            first,
            Some(999),
            "already queued A precedes all 129 B messages"
        );
        assert_eq!(
            a.current_weight(),
            0,
            "returning A releases its original permit"
        );
        a.try_send(1000).expect("same A capacity reusable");
        assert_eq!(rx.recv().await, Some(1000));
        for value in 0..129 {
            assert_eq!(rx.recv().await, Some(value), "B-only progress and FIFO");
        }
        assert_eq!(b.current_weight(), 0);
    }
    #[tokio::test]
    async fn a_priority_and_deferred_shared_credit_preserve_b_fifo() {
        let (a, mut rx) = load_aware_channel(2, UnitWeigher::<u32>::new());
        let b = a.with_budget(2).expect("B pool");
        a.prioritize_primary();
        let credit = b.reserve_independent(1).expect("deferred credit");
        b.try_send_strict(1).expect("B queued");
        assert!(
            b.try_send_strict(2).is_err(),
            "deferred plus output share two credits"
        );
        a.try_send(10).expect("A reserved capacity");
        assert_eq!(rx.recv().await, Some(10));
        b.try_send_prepaid(2, credit)
            .expect("transfer existing credit");
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(b.current_weight(), 0);
    }
    #[tokio::test]
    async fn cancelling_paired_b_drops_data_and_preserves_control_and_a() {
        let (a, mut rx) = load_aware_channel(1, UnitWeigher::<u32>::new());
        let b = a.with_budget(1).expect("B");
        let control = a.with_budget(1).expect("control");
        a.prioritize_primary();
        b.try_send_strict(1).expect("B data");
        control.try_send_strict(2).expect("gap");
        b.cancel_independent();
        a.try_send(3).expect("A capacity");
        assert_eq!(rx.recv().await, Some(3));
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(b.current_weight(), 0);
        assert_eq!(control.current_weight(), 0);
    }
}
