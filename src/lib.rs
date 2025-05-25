#![allow(clippy::result_unit_err)]

use std::{any::TypeId, mem::ManuallyDrop, pin::pin, ptr::NonNull};

use cordyceps::List;
use interface_manager::{InterfaceManager, InterfaceSendError};
use mutex::{BlockingMutex, ConstInit, ScopedRawMutex};
use postcard_rpc::{Endpoint, Key};
use serde::{Serialize, de::DeserializeOwned};
use socket::{SocketHeader, SocketSendError, owned::OwnedSocket};

pub mod interface_manager;
pub mod socket;

struct NetStackInner<M: InterfaceManager> {
    sockets: List<SocketHeader>,
    manager: M,
    port_ctr: u8,
}

impl<M> NetStackInner<M>
where
    M: InterfaceManager,
    M: interface_manager::ConstInit,
{
    pub const fn new() -> Self {
        Self {
            sockets: List::new(),
            port_ctr: 0,
            manager: M::INIT,
        }
    }
}

pub struct NetStack<R: ScopedRawMutex, M: InterfaceManager> {
    inner: BlockingMutex<R, NetStackInner<M>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Address {
    pub network_id: u16,
    pub node_id: u8,
    pub port_id: u8,
}

impl Address {
    pub const fn unknown() -> Self {
        Self {
            network_id: 0,
            node_id: 0,
            port_id: 0,
        }
    }

    #[inline]
    pub fn net_node_any(&self) -> bool {
        self.network_id == 0 && self.node_id == 0
    }
}

#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetStackSendError {
    SocketSend(SocketSendError),
    InterfaceSend(InterfaceSendError),
    NoRoute,
}

impl<R, M> NetStack<R, M>
where
    R: ScopedRawMutex + ConstInit,
    M: InterfaceManager + interface_manager::ConstInit,
{
    pub const fn new() -> Self {
        Self {
            inner: BlockingMutex::new(NetStackInner::new()),
        }
    }
}

impl<R, M> NetStack<R, M>
where
    R: ScopedRawMutex,
    M: InterfaceManager,
{
    pub async fn req_resp<E>(
        &'static self,
        dst: Address,
        req: E::Request,
    ) -> Result<E::Response, NetStackSendError>
    where
        E: Endpoint,
        E::Request: Serialize + DeserializeOwned + 'static,
        E::Response: Serialize + DeserializeOwned + 'static,
    {
        let resp_sock = OwnedSocket::<E::Response>::new(E::RESP_KEY);
        let resp_sock = pin!(resp_sock);
        let mut resp_hdl = resp_sock.attach(self);
        self.send_ty(
            Address {
                network_id: 0,
                node_id: 0,
                port_id: resp_hdl.port(),
            },
            dst,
            E::REQ_KEY,
            req,
        )?;
        let resp = resp_hdl.recv().await;
        Ok(resp.t)
    }

    pub fn send_raw(
        &'static self,
        src: Address,
        dst: Address,
        key: Option<Key>,
        body: &[u8],
    ) -> Result<(), NetStackSendError> {

        let local_bypass = src.net_node_any() && dst.net_node_any();

        self.inner.with_lock(|inner| {
            let res = if !local_bypass {
                inner.manager.send_raw(src, dst, body)
            } else {
                Err(InterfaceSendError::DestinationLocal)
            };

            match res {
                Ok(()) => Ok(()),
                Err(InterfaceSendError::DestinationLocal) => {
                    for socket in inner.sockets.iter_raw() {
                        let (port, vtable, skt_key) = unsafe {
                            let skt_ref = socket.as_ref();
                            let port = *skt_ref.port.get();
                            let vtable = skt_ref.vtable.clone();
                            (port, vtable, skt_ref.key)
                        };
                        // TODO: only allow port_id == 0 if there is only one matching port
                        // with this key.
                        if (port == dst.port_id) || (dst.port_id == 0 && key.is_some_and(|k| k == skt_key)) {
                            let res = {
                                let f = vtable.send_raw;
                                let this: NonNull<SocketHeader> = socket;
                                let this: NonNull<()> = this.cast();
                                // todo
                                (f)(this, body, src, dst).map_err(NetStackSendError::SocketSend)
                            };
                            return res;
                        }
                    }
                    Err(NetStackSendError::NoRoute)
                }
                Err(e) => Err(NetStackSendError::InterfaceSend(e)),
            }
        })
    }
    pub fn send_ty<T: 'static>(
        &'static self,
        src: Address,
        dst: Address,
        key: Key,
        t: T,
    ) -> Result<(), NetStackSendError> {

        // todo: real routing
        assert_eq!(src.network_id, 0);
        assert_eq!(dst.network_id, 0);
        assert_eq!(src.node_id, 0);
        assert_eq!(dst.node_id, 0);
        let mut t = ManuallyDrop::new(t);

        let res = self.inner.with_lock(|inner| {
            for socket in inner.sockets.iter_raw() {
                let (port, vtable, skt_key) = unsafe {
                    let skt_ref = socket.as_ref();
                    let port = *skt_ref.port.get();
                    let vtable = skt_ref.vtable.clone();
                    (port, vtable, skt_ref.key)
                };
                // TODO: only allow port_id == 0 if there is only one matching port
                // with this key.
                // TODO: some kind of distinction of ports that have reasonable return
                // addrs? Should addr just carry the key?
                if (port == dst.port_id || dst.port_id == 0) && key == skt_key {
                    let res = if let Some(f) = vtable.send_owned {
                        let this: NonNull<SocketHeader> = socket;
                        let this: NonNull<()> = this.cast();
                        let that: NonNull<ManuallyDrop<T>> = NonNull::from(&mut t);
                        let that: NonNull<()> = that.cast();
                        (f)(this, that, &TypeId::of::<T>(), src, dst)
                    } else if let Some(_f) = vtable.send_bor {
                        todo!()
                    } else {
                        // keep going?
                        Err(SocketSendError::WhatTheHell)
                    };
                    return Some(res);
                }
            }
            None
        });

        // If we didn't ever take the item, we need to drop it
        if res.is_none() || res.as_ref().is_some_and(|r| r.is_err()) {
            unsafe {
                ManuallyDrop::drop(&mut t);
            }
        }

        if let Some(r) = res {
            r.map_err(NetStackSendError::SocketSend)
        } else {
            Err(NetStackSendError::NoRoute)
        }
    }
    pub(crate) unsafe fn attach_socket(&'static self, node: NonNull<SocketHeader>) -> u8 {
        self.inner.with_lock(|inner| {
            // TODO: smarter than this, do something like littlefs2's "next free block"
            // bitmap thing?
            let start = inner.port_ctr;
            loop {
                inner.port_ctr = inner.port_ctr.wrapping_add(1).max(1);
                let exists = inner.sockets.iter().any(|s| {
                    let port = unsafe { *s.port.get() };
                    port == inner.port_ctr
                });
                if !exists {
                    break;
                } else if inner.port_ctr == start {
                    panic!("exhausted all addrs");
                }
            }
            unsafe {
                node.as_ref().port.get().write(inner.port_ctr);
            }

            inner.sockets.push_front(node);
            inner.port_ctr
        })
    }
}

impl<R, M> Default for NetStack<R, M>
where
    R: ScopedRawMutex + ConstInit,
    M: InterfaceManager + interface_manager::ConstInit,
{
    fn default() -> Self {
        Self::new()
    }
}

// TODO: Routing table, what does it do?
// TODO: Socket vtable, what does it do?
//   - process type t
//   - process bytes &[u8]
//   - for both:
//     - do we remove/consume the socket
//     - did the receive succeed or not
//     - take a waker (optional)

// TODO:
//
// We should have some netstack-level equivalent of `req_resp`.
//   it should:
//
// 1. register the RECEPTION socket
//   * oneshot
//   * ACTUALLY "response || error || None" type
// 2. attempt to send the request
//   * if success, await socket rx
//   * if fail, return, ensure that drop is enough to remove
//       the listening socket
