use core::cell::UnsafeCell;
use std::{
    any::TypeId,
    collections::VecDeque,
    marker::PhantomData,
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll, Waker},
};

use cordyceps::list::Links;
use mutex::ScopedRawMutex;
use postcard_rpc::{Endpoint, Topic};
use serde::{Serialize, de::DeserializeOwned};

use crate::{Address, NetStack, interface_manager::InterfaceManager};

use super::{OwnedMessage, SocketHeader, SocketSendError, SocketTy, SocketVTable};

// Owned Socket
#[repr(C)]
pub struct StdBoundedSocket<T>
where
    T: Serialize + DeserializeOwned + 'static,
{
    // LOAD BEARING: must be first
    hdr: SocketHeader,
    // TODO: just a single item, we probably want a more ring-buffery
    // option for this.
    inner: UnsafeCell<BoundedQueue<T>>,
}

pub struct StdBoundedSocketHdl<'a, T, R, M>
where
    T: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
    pub(crate) ptr: NonNull<StdBoundedSocket<T>>,
    _lt: PhantomData<Pin<&'a mut StdBoundedSocket<T>>>,
    pub(crate) net: &'static NetStack<R, M>,
    port: u8,
}

pub struct Recv<'a, 'b, T, R, M>
where
    T: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
    hdl: &'a mut StdBoundedSocketHdl<'b, T, R, M>,
}

struct BoundedQueue<T: 'static> {
    wait: Option<Waker>,
    // TODO: We could probably do better than a VecDeque with a boxed slice
    // and a ringbuffer, which could maybe also be shared with the std
    // inline buffer, but for now this is fine.
    queue: VecDeque<OwnedMessage<T>>,
    max_len: usize,
}

// ---- impls ----

// impl OwnedMessage

// ...

// impl StdBoundedSocket

impl<T> StdBoundedSocket<T>
where
    T: Serialize + DeserializeOwned + 'static,
{
    pub fn new_topic_in<U: Topic>(bound: usize) -> Self {
        Self {
            hdr: SocketHeader {
                links: Links::new(),
                vtable: const { &Self::vtable() },
                port: 0,
                kind: const { SocketTy::topic_in::<U>() },
            },
            inner: UnsafeCell::new(BoundedQueue::new(bound)),
        }
    }

    pub fn new_endpoint_req<E: Endpoint>(bound: usize) -> Self {
        Self {
            hdr: SocketHeader {
                links: Links::new(),
                vtable: const { &Self::vtable() },
                port: 0,
                kind: const { SocketTy::endpoint_req::<E>() },
            },
            inner: UnsafeCell::new(BoundedQueue::new(bound)),
        }
    }

    pub fn new_endpoint_resp<E: Endpoint>(bound: usize) -> Self {
        Self {
            hdr: SocketHeader {
                links: Links::new(),
                vtable: const { &Self::vtable() },
                port: 0,
                kind: const { SocketTy::endpoint_resp::<E>() },
            },
            inner: UnsafeCell::new(BoundedQueue::new(bound)),
        }
    }

    pub fn attach<'a, R: ScopedRawMutex + 'static, M: InterfaceManager + 'static>(
        self: Pin<&'a mut Self>,
        stack: &'static NetStack<R, M>,
    ) -> StdBoundedSocketHdl<'a, T, R, M> {
        let ptr_self: NonNull<Self> = NonNull::from(unsafe { self.get_unchecked_mut() });
        let ptr_erase: NonNull<SocketHeader> = ptr_self.cast();
        let port = unsafe { stack.attach_socket(ptr_erase) };
        StdBoundedSocketHdl {
            ptr: ptr_self,
            _lt: PhantomData,
            net: stack,
            port,
        }
        // TODO: once-check?
    }

    const fn vtable() -> SocketVTable {
        SocketVTable {
            send_owned: Some(Self::send_owned),
            // TODO: We probably COULD support this, but I'm pretty sure it
            // would require serializing, copying to a buffer, then later
            // deserializing. I really don't know if we WANT this.
            send_bor: None,
            send_raw: Self::send_raw,
        }
    }

    fn send_owned(
        this: NonNull<()>,
        that: NonNull<()>,
        ty: &TypeId,
        src: Address,
        dst: Address,
        seq: u16,
    ) -> Result<(), SocketSendError> {
        if &TypeId::of::<T>() != ty {
            debug_assert!(false, "Type Mismatch!");
            return Err(SocketSendError::TypeMismatch);
        }
        let that: NonNull<T> = that.cast();
        let this: NonNull<Self> = this.cast();
        let this: &Self = unsafe { this.as_ref() };
        let mutitem: &mut BoundedQueue<T> = unsafe { &mut *this.inner.get() };

        if mutitem.queue.len() >= mutitem.max_len {
            return Err(SocketSendError::NoSpace);
        }

        mutitem.queue.push_back(OwnedMessage {
            src,
            dst,
            t: unsafe { that.read() },
            seq,
        });
        if let Some(w) = mutitem.wait.take() {
            w.wake();
        }

        Ok(())
    }

    // fn send_bor(
    //     this: NonNull<()>,
    //     that: NonNull<()>,
    //     src: Address,
    //     dst: Address,
    // ) -> Result<(), ()> {
    //     // I don't think we can support this?
    //     Err(())
    // }

    fn send_raw(
        this: NonNull<()>,
        that: &[u8],
        src: Address,
        dst: Address,
        seq: u16,
    ) -> Result<(), SocketSendError> {
        let this: NonNull<Self> = this.cast();
        let this: &Self = unsafe { this.as_ref() };
        let mutitem: &mut BoundedQueue<T> = unsafe { &mut *this.inner.get() };

        if mutitem.queue.len() >= mutitem.max_len {
            return Err(SocketSendError::NoSpace);
        }

        if let Ok(t) = postcard::from_bytes::<T>(that) {
            mutitem.queue.push_back(OwnedMessage { src, dst, t, seq });
            if let Some(w) = mutitem.wait.take() {
                w.wake();
            }
            Ok(())
        } else {
            Err(SocketSendError::DeserFailed)
        }
    }
}

impl<T: Serialize + DeserializeOwned + 'static> Drop for StdBoundedSocket<T> {
    fn drop(&mut self) {
        unsafe {
            core::ptr::drop_in_place(self.inner.get());
        }
    }
}

// impl StdBoundedSocketHdl

// TODO: impl drop, remove waker, remove socket
impl<'a, T, R, M> StdBoundedSocketHdl<'a, T, R, M>
where
    T: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
    pub fn port(&self) -> u8 {
        self.port
    }

    // TODO: This future is !Send? I don't fully understand why, but rustc complains
    // that since `NonNull<StdBoundedSocket<E>>` is !Sync, then this future can't be Send,
    // BUT impl'ing Sync unsafely on StdBoundedSocketHdl + StdBoundedSocket doesn't seem to help.
    pub fn recv<'b>(&'b mut self) -> Recv<'b, 'a, T, R, M> {
        Recv { hdl: self }
    }
}

impl<T, R, M> Drop for StdBoundedSocketHdl<'_, T, R, M>
where
    T: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
    fn drop(&mut self) {
        println!("Dropping StdBoundedSocketHdl!");
        // first things first, remove the item from the list
        self.net.inner.with_lock(|net| {
            let node: NonNull<StdBoundedSocket<T>> = self.ptr;
            let node: NonNull<SocketHeader> = node.cast();
            unsafe {
                net.sockets.remove(node);
            }
        });
    }
}

unsafe impl<T, R, M> Send for StdBoundedSocketHdl<'_, T, R, M>
where
    T: Send,
    T: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
}

unsafe impl<T, R, M> Sync for StdBoundedSocketHdl<'_, T, R, M>
where
    T: Send,
    T: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
}

// impl Recv

impl<T, R, M> Future for Recv<'_, '_, T, R, M>
where
    T: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
    type Output = OwnedMessage<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = self.hdl.net.inner.with_lock(|_net| {
            let this_ref: &StdBoundedSocket<T> = unsafe { self.hdl.ptr.as_ref() };
            let box_ref: &mut BoundedQueue<T> = unsafe { &mut *this_ref.inner.get() };
            if let Some(t) = box_ref.queue.pop_front() {
                Some(t)
            } else {
                let new_wake = cx.waker();
                if let Some(w) = box_ref.wait.take() {
                    if !w.will_wake(new_wake) {
                        w.wake();
                    }
                }
                // NOTE: Okay to register waker AFTER checking, because we
                // have an exclusive lock
                box_ref.wait = Some(new_wake.clone());
                None
            }
        });
        if let Some(t) = res {
            Poll::Ready(t)
        } else {
            Poll::Pending
        }
    }
}

unsafe impl<T, R, M> Sync for Recv<'_, '_, T, R, M>
where
    T: Send,
    T: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
}

// impl BoundedQueue

impl<T: 'static> BoundedQueue<T> {
    fn new(bound: usize) -> Self {
        Self {
            wait: None,
            queue: VecDeque::new(),
            max_len: bound,
        }
    }
}
