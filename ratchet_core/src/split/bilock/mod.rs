// Copyright 2015-2021 Swim Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[cfg(test)]
mod tests;

use core::fmt;
use futures::task::AtomicWaker;
use futures::{ready, Future};
use std::cell::UnsafeCell;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Creates a new BiLock that provides exclusive access to the value.
///
/// For situations where there will only ever be two owners this is much cheaper than a mutex.
pub fn bilock<T>(val: T) -> (BiLock<T>, BiLock<T>) {
    let inner = Arc::new(Inner {
        state: AtomicBool::new(false),
        waker: AtomicWaker::default(),
        value: UnsafeCell::new(val),
    });
    (
        BiLock {
            inner: inner.clone(),
        },
        BiLock { inner },
    )
}

/// BiLock
#[derive(Debug)]
pub struct BiLock<T> {
    inner: Arc<Inner<T>>,
}

#[derive(Debug)]
struct Inner<T> {
    state: AtomicBool,
    waker: AtomicWaker,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

impl<T> BiLock<T> {
    /// Returns true if the two BiLock point to the same allocation
    pub fn same_bilock(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Returns a future that resolves to a `BiLockGuard` once the value is available.
    pub fn lock(&self) -> LockFuture<'_, T> {
        LockFuture { bilock: self }
    }

    /// Polls access to the value and returns a `BiLockGuard` if it is available.
    pub fn poll_lock(&self, cx: &mut Context<'_>) -> Poll<BiLockGuard<'_, T>> {
        if self.inner.state.swap(true, Ordering::SeqCst) {
            self.inner.waker.register(cx.waker());
            if self.inner.state.swap(true, Ordering::SeqCst) {
                Poll::Pending
            } else {
                Poll::Ready(BiLockGuard { bilock: self })
            }
        } else {
            Poll::Ready(BiLockGuard { bilock: self })
        }
    }

    fn unlock(&self) {
        if self.inner.state.swap(false, Ordering::SeqCst) {
            self.inner.waker.wake();
        } else {
            // Nobody is waiting
        }
    }

    /// Reunites two `BiLock`s that form a pair or returns an error if they do not guard the same
    /// value.
    pub fn reunite(self, other: BiLock<T>) -> Result<T, ReuniteError<BiLock<T>>>
    where
        T: Unpin,
    {
        if Arc::ptr_eq(&self.inner, &other.inner) {
            drop(other);

            let inner = Arc::try_unwrap(self.inner)
                .ok()
                .expect("Failed to unwrap Arc");
            Ok(inner.value.into_inner())
        } else {
            Err(ReuniteError(self, other))
        }
    }
}

#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct LockFuture<'l, T> {
    bilock: &'l BiLock<T>,
}

impl<T> Unpin for LockFuture<'_, T> {}

impl<'l, T> Future for LockFuture<'l, T> {
    type Output = BiLockGuard<'l, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().bilock.poll_lock(cx)
    }
}

pub struct BiLockGuard<'l, T> {
    bilock: &'l BiLock<T>,
}

impl<'l, T> Drop for BiLockGuard<'l, T> {
    fn drop(&mut self) {
        self.bilock.unlock();
    }
}

impl<T> Deref for BiLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.bilock.inner.value.get() }
    }
}

impl<T: Unpin> DerefMut for BiLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.bilock.inner.value.get() }
    }
}

pub struct ReuniteError<T>(pub T, pub T);

impl<T> Debug for ReuniteError<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ReuniteError").finish()
    }
}

impl<T> Display for ReuniteError<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Attempted to reunite two BiLocks that don't form a pair")
    }
}

impl<T> Error for ReuniteError<T> {}

impl<T> AsyncRead for BiLock<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut guard = ready!(self.get_mut().poll_lock(cx));
        Pin::new(guard.deref_mut()).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for BiLock<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let mut guard = ready!(self.get_mut().poll_lock(cx));
        Pin::new(guard.deref_mut()).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let mut guard = ready!(self.get_mut().poll_lock(cx));
        Pin::new(guard.deref_mut()).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let mut guard = ready!(self.get_mut().poll_lock(cx));
        Pin::new(guard.deref_mut()).poll_flush(cx)
    }
}
