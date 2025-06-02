use core::any::TypeId;
use core::ptr::{self, NonNull};
use std::cell::UnsafeCell;

use cordyceps::{Linked, list::Links};
use postcard_rpc::Key;

use crate::Address;

pub mod endpoint;
pub mod owned;

pub struct SocketHeader {
    pub(crate) links: Links<SocketHeader>,
    pub(crate) key: Key,
    pub(crate) port: UnsafeCell<u8>,
    pub(crate) vtable: SocketVTable, // &Vtable?
}

// TODO: Way of signaling "socket consumed"?
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SocketSendError {
    NoSpace,
    DeserFailed,
    TypeMismatch,
    WhatTheHell,
}

// Morally: &mut ManuallyDrop<T>, TypeOf<T>, src, dst
// If return OK: the type has been moved OUT of the source
// May serialize, or may be just moved.
pub type SendOwned = fn(
    // The socket ptr
    NonNull<()>,
    // The T ptr
    NonNull<()>,
    // The T ty
    &TypeId,
    // The dst
    Address,
    // the src
    Address,
    // the seq_no
    u16,
) -> Result<(), SocketSendError>;
// Morally: &T, src, dst
// Always a serialize
pub type SendBorrowed = fn(
    // The socket ptr
    NonNull<()>,
    // The T ptr
    NonNull<()>,
    // the src
    Address,
    // The dst
    Address,
    // the seq_no
    u16,
) -> Result<(), SocketSendError>;
// Morally: it's a packet
// Never a serialize, sometimes a deserialize
pub type SendRaw = fn(
    // The socket ptr
    NonNull<()>,
    // The packet
    &[u8],
    // The src
    Address,
    // The dst
    Address,
    // the seq_no
    u16,
) -> Result<(), SocketSendError>;

#[derive(Clone)]
pub struct SocketVTable {
    pub(crate) send_owned: Option<SendOwned>,
    pub(crate) send_bor: Option<SendBorrowed>,
    pub(crate) send_raw: SendRaw,
    // NOTE: We do *not* have a `drop` impl here, because the list
    // doesn't ACTUALLY own the nodes, so it is not responsible for dropping
    // them. They are naturally destroyed by their true owner.
}

// --------------------------------------------------------------------------
// impl SocketHeader
// --------------------------------------------------------------------------

unsafe impl Linked<Links<SocketHeader>> for SocketHeader {
    type Handle = NonNull<SocketHeader>;

    fn into_ptr(r: Self::Handle) -> std::ptr::NonNull<Self> {
        r
    }

    unsafe fn from_ptr(ptr: std::ptr::NonNull<Self>) -> Self::Handle {
        ptr
    }

    unsafe fn links(target: NonNull<Self>) -> NonNull<Links<SocketHeader>> {
        // Safety: using `ptr::addr_of!` avoids creating a temporary
        // reference, which stacked borrows dislikes.
        let node = unsafe { ptr::addr_of_mut!((*target.as_ptr()).links) };
        unsafe { NonNull::new_unchecked(node) }
    }
}
