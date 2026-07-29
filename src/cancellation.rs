use std::{
    future::Future,
    io, mem,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};

pub struct CancelableIo<T>(T, CancelSignal);
impl<R: io::Read> io::Read for CancelableIo<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.1.as_error()?;
        self.0.read(buf)
    }

    fn read_vectored(&mut self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
        self.1.as_error()?;
        self.0.read_vectored(bufs)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.1.as_error()?;
        self.0.read_exact(buf)
    }
}
impl<R: io::BufRead> io::BufRead for CancelableIo<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.1.as_error()?;
        self.0.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        self.0.consume(amt)
    }
}
impl<W: io::Write> io::Write for CancelableIo<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.1.as_error()?;
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

pub struct WaitForCancelSignalFuture {
    signal: CancelSignal,
    index: usize,
}
impl WaitForCancelSignalFuture {
    pub fn new(signal: CancelSignal) -> Self {
        Self {
            signal,
            index: usize::MAX,
        }
    }
    /// Forget the waker that this future guards. The stored waker will remain
    /// connected to the [`CancelSignal`] until that signal is canceled.
    pub fn forget(mut self) {
        self.index = usize::MAX;
        drop(self);
    }
    /// Converts a closure into a custom `Waker` and then stores it using this
    /// types `set_waker` method. Returns `Err` if the wrapped `CancelSignal`
    /// was already canceled.
    pub fn set_waker_from_closure(
        &mut self,
        f: impl FnOnce() + Send + Sync + 'static,
    ) -> io::Result<()> {
        struct FnWaker<F>(F);
        impl<F> Wake for FnWaker<F>
        where
            F: FnOnce(),
        {
            fn wake(self: Arc<Self>) {
                match Arc::try_unwrap(self) {
                    Ok(f) => (f.0)(),
                    Err(_) => unreachable!(
                        "A waker stored inside a {} were cloned at least once",
                        stringify!(CancelSignal)
                    ),
                }
            }

            fn wake_by_ref(self: &Arc<Self>) {
                unreachable!(
                    "{} tried to wake a Waker by reference",
                    stringify!(CancelSignal)
                )
            }
        }

        self.set_waker(Arc::new(FnWaker(f)).into())
            .map_err(|_| CancelSignal::error_with_cancel_reason(self.signal.reason().as_deref()))
    }
    /// Sets the waker that this future guards. Any waker that was previously set
    /// will be dropped.
    ///
    /// If [`Future::poll`] is called or if this method is called again then the
    /// provided waker will be overwritten and dropped.
    ///
    /// When the [`WaitForCancelSignalFuture`] future is dropped the set waker will
    /// be unregistered and dropped.
    ///
    /// If the wrapped [`CancelSignal`] is canceled before this method is called
    /// then the provided waker will be returned in an `Err` variant. Otherwise
    /// `Ok(())` is returned and the provided waker will be woken when the wrapped
    /// signal is canceled.
    pub fn set_waker(&mut self, waker: Waker) -> Result<(), Waker> {
        // Ensure we don't run drop logic while holding a lock:
        let _removed_waker;
        {
            let mut guard = self.signal.inner.state.lock().unwrap();
            if guard.canceled {
                Err(waker)
            } else {
                if self.index == usize::MAX {
                    // No index/slot reserved, find an empty one:
                    for (index, slot) in guard.wakers.iter_mut().enumerate() {
                        if slot.is_none() {
                            self.index = index;
                            // Slot is empty so this won't run any drop logic:
                            *slot = Some(waker);
                            return Ok(());
                        }
                    }
                    self.index = guard.wakers.len();
                    guard.wakers.push(Some(waker));
                } else {
                    _removed_waker = guard.wakers[self.index].replace(waker);
                }
                Ok(())
            }
        }
    }
}
impl Future for WaitForCancelSignalFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        if this.signal.check() {
            return Poll::Ready(());
        }
        // Don't clone waker while holding a lock (so we must clone it before acquiring it):
        let waker = cx.waker().clone();
        match this.set_waker(waker) {
            Ok(()) => Poll::Pending,
            Err(_) => Poll::Ready(()),
        }
    }
}
impl Drop for WaitForCancelSignalFuture {
    fn drop(&mut self) {
        if self.index == usize::MAX {
            // Haven't reserved a slot for a waker yet.
            return;
        }
        if self.signal.check() {
            // No wakers will be stored inside the signal:
            return;
        }
        let _removed_waker;
        {
            if let Ok(mut guard) = self.signal.inner.state.lock() {
                if guard.canceled {
                    // No wakers will be stored:
                    return;
                }
                if let Some(slot) = guard.wakers.get_mut(self.index) {
                    // Don't run drop logic while holding lock:
                    _removed_waker = slot.take();
                }
            }
        }
    }
}

struct CancelSignalState {
    /// `true` if the user of the signal should cancel.
    canceled: bool,
    /// A message that indicates the reason that the signal was canceled.
    reason: String,
    /// Wakers that should be woken up when this signal is canceled.
    wakers: Vec<Option<Waker>>,
    /// Handles for "callbacks" enqueued in parent signals.
    parents: Vec<WaitForCancelSignalFuture>,
}
struct CancelSignalShared {
    /// This might lag slightly after the `canceled` field in `state` but it is
    /// quicker to check.
    quick: AtomicBool,
    state: Mutex<CancelSignalState>,
    condvar: Condvar,
}
impl CancelSignalShared {
    fn _cancel_with_reason(&self, reason: Option<String>) {
        let wakers;
        let parents;
        {
            let mut guard = self.state.lock().unwrap();
            if guard.canceled {
                return;
            }
            guard.canceled = true;
            if let Some(reason) = reason {
                guard.reason = reason;
            }
            // Don't use these while we are holding the lock:
            wakers = mem::take(&mut guard.wakers);
            parents = mem::take(&mut guard.parents);

            self.quick.store(true, Ordering::Release);
        }
        self.condvar.notify_all();

        // Remove callbacks in parent signals:
        drop(parents);

        // Notify any futures/callbacks:
        for waker in wakers.into_iter().flatten() {
            waker.wake();
        }
    }
}
#[derive(Clone)]
pub struct CancelSignal {
    inner: Arc<CancelSignalShared>,
}
impl CancelSignal {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancelSignalShared {
                quick: AtomicBool::new(false),
                state: Mutex::new(CancelSignalState {
                    canceled: false,
                    reason: String::new(),
                    wakers: Vec::new(),
                    parents: Vec::new(),
                }),
                condvar: Condvar::new(),
            }),
        }
    }

    /// Wrap the signal in a type that will cancel it when the type is dropped.
    pub fn cancel_on_drop(self) -> CancelOnDrop {
        CancelOnDrop::new(self)
    }

    /// Notify this signal that it should cancel.
    pub fn cancel(&self) {
        self.inner._cancel_with_reason(None);
    }
    /// Notify this signal that it should cancel.
    pub fn cancel_with_reason(&self, reason: impl Into<String>) {
        self.inner._cancel_with_reason(Some(reason.into()));
    }

    /// The reason that the signal was canceled.
    pub fn reason(&self) -> Option<String> {
        Some(self.inner.state.lock().unwrap().reason.clone()).filter(|text| !text.is_empty())
    }
    /// Check if the signal has been canceled. Returns `true` if the operation should
    /// be canceled.
    pub fn check(&self) -> bool {
        // Ensure there is no strange ordering, such as Out-of-thin-air (OOTA),
        // see https://paulmck.livejournal.com/63517.html
        //
        // This shouldn't be an issue in most cases since a signal usually won't
        // be `cancel`ed in response to a `check` call and so the signal should
        // count as "Independent Input Data" to whatever algorithm uses it, see
        // "2.6.1 Independent Input Data" in the paper "P2055R0: A Relaxed Guide
        // to memory_order_relaxed" at
        // http://www.open-std.org/jtc1/sc22/wg21/docs/papers/2020/p2055r0.pdf
        //
        // But since we can't know how this signal will actually be used we
        // better use `Acquire` ordering instead of `Relaxed` ordering to
        // prevent OOTA. It compiles to the same instructions on x86 anyway and
        // should only be slightly slower on ARM.
        self.inner.quick.load(Ordering::Acquire)
    }
    /// Return an error if the signal has been canceled.
    pub fn as_error(&self) -> io::Result<()> {
        if self.check() {
            Err(Self::error_with_cancel_reason(self.reason().as_deref()))
        } else {
            Ok(())
        }
    }
    pub fn error_with_cancel_reason(reason: Option<&str>) -> io::Error {
        if let Some(reason) = reason {
            io::Error::other(format!("Operation canceled in response to {}", reason))
        } else {
            io::Error::other("Operation canceled")
        }
    }
    /// Wait while this signal is not canceled. Returns `Err(())` if the signal was
    /// canceled.
    pub fn wait_timeout(&self, duration: Duration) -> io::Result<()> {
        let guard = self.inner.state.lock().unwrap();
        let (guard, result) = self
            .inner
            .condvar
            // Wait on timeout while signal isn't canceled:
            .wait_timeout_while(guard, duration, |state| !state.canceled)
            // Unwrap poison:
            .unwrap();
        drop(guard);
        if result.timed_out() {
            // Not canceled while waiting for timeout duration:
            Ok(())
        } else {
            Err(Self::error_with_cancel_reason(self.reason().as_deref()))
        }
    }

    /// If the parent signal is canceled then this signal should be canceled as well.
    pub fn add_parent_signal(&self, parent: &Self) {
        if parent.check() {
            self.inner._cancel_with_reason(parent.reason());
            return;
        }

        // Enqueue a new callback to the parent (this will briefly lock the parent when `set_waker` is called):
        let mut handle = WaitForCancelSignalFuture::new(parent.clone());
        let set_waker_result = handle.set_waker_from_closure({
            let this = self.clone();
            let parent = parent.clone();
            move || {
                this.inner._cancel_with_reason(parent.reason());
            }
        });
        if set_waker_result.is_err() {
            // Parent already canceled:
            self.inner._cancel_with_reason(parent.reason());
            return;
        }

        // Lock the child and store the callback handle so that the callback can be removed
        // if the child is canceled:
        {
            let mut guard = self.inner.state.lock().unwrap();
            if guard.canceled {
                // Child canceled:
                return;
            }
            let already_added = guard
                .parents
                .iter()
                .any(|a_parent_handle| Arc::ptr_eq(&a_parent_handle.signal.inner, &parent.inner));
            if already_added {
                return;
            }
            // Remember this parent so we can unlink if this "child" signal is canceled:
            guard.parents.push(handle);
        }
    }

    /// Create a new signal that will be canceled if the current signal is but that
    /// can also be canceled separately without affecting the current signal.
    pub fn new_child_signal(&self) -> CancelSignal {
        let signal = CancelSignal::new();
        signal.add_parent_signal(self);
        signal
    }

    /// Wrap an io type so that it will return errors if it is used after this
    /// signal has been canceled.
    pub fn wrap_io<T>(&self, io_type: T) -> CancelableIo<T> {
        CancelableIo(io_type, self.clone())
    }

    /// Returns a future that can be used to wait for cancellation.
    ///
    /// The future has some useful methods, for example
    /// [`WaitForCancelSignalFuture::set_waker_from_closure`] that allows
    /// running a callback when the signal is cancelled. The callback will be
    /// canceled if the future is dropped.
    pub fn wait_future(&self) -> WaitForCancelSignalFuture {
        WaitForCancelSignalFuture::new(self.clone())
    }
}
impl Default for CancelSignal {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CancelOnDrop(Option<CancelSignal>);
impl CancelOnDrop {
    pub fn new(signal: CancelSignal) -> Self {
        Self(Some(signal))
    }
    pub fn disarm(mut self) -> CancelSignal {
        self.0.take().unwrap()
    }
}
impl Deref for CancelOnDrop {
    type Target = CancelSignal;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}
impl DerefMut for CancelOnDrop {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(signal) = &self.0 {
            signal.cancel();
        }
    }
}
