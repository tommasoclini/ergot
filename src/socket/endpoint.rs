// use core::marker::PhantomData;
use core::cell::UnsafeCell;
use std::{any::TypeId, future::poll_fn, marker::PhantomData, pin::Pin, ptr::NonNull, task::{Context, Poll, Waker}};

use cordyceps::list::Links;
use mutex::ScopedRawMutex;
use postcard_rpc::Endpoint;
use serde::{Serialize, de::DeserializeOwned};

use crate::{Address, NetStack};

use super::{SocketHeader, SocketVTable};

#[derive(Debug, PartialEq)]
pub struct OwnedMessage<T: 'static> {
    pub src: Address,
    pub dst: Address,
    pub t: T,
}

struct OneBox<T: 'static> {
    wait: Option<Waker>,
    t: Option<OwnedMessage<T>>
}

impl<T: 'static> OneBox<T> {
    const fn new() -> Self {
        Self {
            wait: None,
            t: None,
        }
    }
}

impl<T: 'static> Default for OneBox<T> {
    fn default() -> Self {
        Self::new()
    }
}

// Owned Endpoint Server Socket
#[repr(C)]
pub struct OwnedSocket<E>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static
{
    // LOAD BEARING: must be first
    hdr: SocketHeader,
    // TODO: just a single item, we probably want a more ring-buffery
    // option for this.
    inner: UnsafeCell<OneBox<E::Request>>,
}

pub struct OwnedSocketHdl<'a, E, R>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
{
    ptr: NonNull<OwnedSocket<E>>,
    _lt: PhantomData<&'a OwnedSocket<E>>,
    net: &'static NetStack<R>,
}

// TODO: impl drop, remove waker, remove socket
impl<E, R> OwnedSocketHdl<'_, E, R>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
{
    pub async fn recv(&mut self) -> OwnedMessage<E::Request> {
        poll_fn(|cx: &mut Context<'_>| {
            let res = self.net.inner.with_lock(|_net| {
                let this_ref: &OwnedSocket<E> = unsafe { self.ptr.as_ref() };
                let box_ref: &mut OneBox<E::Request> = unsafe { &mut *this_ref.inner.get() };
                if let Some(t) = box_ref.t.take() {
                    Some(t)
                } else {
                    // todo
                    assert!(box_ref.wait.is_none());
                    // NOTE: Okay to register waker AFTER checking, because we
                    // have an exclusive lock
                    box_ref.wait = Some(cx.waker().clone());
                    None
                }
            });
            if let Some(t) = res {
                Poll::Ready(t)
            } else {
                Poll::Pending
            }
        }).await
    }
}

impl<E> OwnedSocket<E>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
{
    pub const fn new() -> Self {
        Self {
            hdr: SocketHeader {
                links: Links::new(),
                vtable: Self::vtable(),
                key: E::REQ_KEY,
                port: UnsafeCell::new(0),
            },
            inner: UnsafeCell::new(OneBox::new()),
        }
    }

    pub fn attach<'a, R: ScopedRawMutex + 'static>(
        self: Pin<&'a mut Self>,
        stack: &'static NetStack<R>,
    ) -> OwnedSocketHdl<'a, E, R> {
        let ptr_self: NonNull<Self> = NonNull::from(unsafe { self.get_unchecked_mut() });
        let ptr_erase: NonNull<SocketHeader> = ptr_self.cast();
        unsafe {
            stack.attach_socket(ptr_erase);
        }
        OwnedSocketHdl {
            ptr: ptr_self,
            _lt: PhantomData,
            net: stack,
        }
        // TODO: once-check?
    }

    const fn vtable() -> SocketVTable {
        SocketVTable {
            send_owned: Some(Self::send_owned),
            send_bor: None,
            send_raw: Some(Self::send_raw),
        }
    }

    fn send_owned(
        this: NonNull<()>,
        that: NonNull<()>,
        ty: &TypeId,
        src: Address,
        dst: Address,
    ) -> Result<(), ()> {
        if &TypeId::of::<E::Request>() != ty {
            debug_assert!(false, "Type Mismatch!");
            return Err(());
        }
        let that: NonNull<E::Request> = that.cast();
        let this: NonNull<Self> = this.cast();
        let this: &Self = unsafe { this.as_ref() };
        let mutitem: &mut OneBox<E::Request> = unsafe { &mut *this.inner.get() };

        if mutitem.t.is_some() {
            return Err(());
        }

        mutitem.t = Some(OwnedMessage {
            src,
            dst,
            t: unsafe { that.read() },
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

    fn send_raw(this: NonNull<()>, that: &[u8], src: Address, dst: Address) -> Result<(), ()> {
        let this: NonNull<Self> = this.cast();
        let this: &Self = unsafe { this.as_ref() };
        let mutitem: &mut OneBox<E::Request> = unsafe { &mut *this.inner.get() };

        if mutitem.t.is_some() {
            return Err(());
        }

        if let Ok(t) = postcard::from_bytes::<E::Request>(that) {
            mutitem.t = Some(OwnedMessage { src, dst, t });
            if let Some(w) = mutitem.wait.take() {
                w.wake();
            }
            Ok(())
        } else {
            Err(())
        }
    }
}

impl<E> Default for OwnedSocket<E>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}
