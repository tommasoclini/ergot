// use core::marker::PhantomData;
use core::cell::UnsafeCell;
use std::{any::TypeId, marker::PhantomData, pin::Pin, ptr::NonNull};

use cordyceps::list::Links;
use mutex::ScopedRawMutex;
use serde::{Serialize, de::DeserializeOwned};

use crate::{Address, NetStack};

use super::{SocketHeader, SocketVTable};

pub struct OwnedMessage<T: 'static> {
    src: Address,
    dst: Address,
    t: T,
}

// Owned Endpoint Server Socket
#[repr(C)]
pub struct OwnedSocket<T: Serialize + DeserializeOwned + 'static> {
    // LOAD BEARING: must be first
    hdr: SocketHeader,
    // TODO: just a single item, we probably want a more ring-buffery
    // option for this.
    _t: UnsafeCell<Option<OwnedMessage<T>>>,
}

pub struct OwnedSocketHdl<'a, T, R>
where
    T: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
{
    ptr: NonNull<OwnedSocket<T>>,
    _lt: PhantomData<&'a OwnedSocket<T>>,
    net: &'static NetStack<R>,
}

impl<T> OwnedSocket<T>
where
    T: Serialize + DeserializeOwned + 'static,
{
    pub const fn new() -> Self {
        Self {
            hdr: SocketHeader {
                links: Links::new(),
                vtable: Self::vtable(),
                kty: [0u8; 8], // TODO: generic over E: Endpoint
                port: UnsafeCell::new(0),
            },
            _t: UnsafeCell::new(None),
        }
    }

    pub fn attach<'a, R: ScopedRawMutex + 'static>(
        self: Pin<&'a mut Self>,
        stack: &'static NetStack<R>,
    ) -> OwnedSocketHdl<'a, T, R> {
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
        dst: Address,
        src: Address,
    ) -> Result<(), ()> {
        if &TypeId::of::<T>() != ty {
            debug_assert!(false, "Type Mismatch!");
            return Err(());
        }
        let that: NonNull<T> = that.cast();
        let this: NonNull<Self> = this.cast();
        let this: &Self = unsafe { this.as_ref() };
        let mutitem: &mut Option<OwnedMessage<T>> = unsafe { &mut *this._t.get() };

        if mutitem.is_some() {
            return Err(());
        }

        *mutitem = Some(OwnedMessage {
            src,
            dst,
            t: unsafe { that.read() },
        });

        Ok(())
    }

    // fn send_bor(
    //     this: NonNull<()>,
    //     that: NonNull<()>,
    //     dst: Address,
    //     src: Address,
    // ) -> Result<(), ()> {
    //     // I don't think we can support this?
    //     Err(())
    // }

    fn send_raw(this: NonNull<()>, that: &[u8], dst: Address, src: Address) -> Result<(), ()> {
        let this: NonNull<Self> = this.cast();
        let this: &Self = unsafe { this.as_ref() };
        let mutitem: &mut Option<OwnedMessage<T>> = unsafe { &mut *this._t.get() };

        if mutitem.is_some() {
            return Err(());
        }

        if let Ok(t) = postcard::from_bytes::<T>(that) {
            *mutitem = Some(OwnedMessage { src, dst, t });
            Ok(())
        } else {
            Err(())
        }
    }
}

impl<T> Default for OwnedSocket<T>
where
    T: Serialize + DeserializeOwned + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}
