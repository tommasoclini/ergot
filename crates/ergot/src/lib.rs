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
pub mod well_known;

struct NetStackInner<M: InterfaceManager> {
    sockets: List<SocketHeader>,
    manager: M,
    port_ctr: u8,
    seq_no: u16,
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
            seq_no: 0,
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

    #[inline]
    pub fn as_u32(&self) -> u32 {
        ((self.network_id as u32) << 16) | ((self.node_id as u32) << 8) | (self.port_id as u32)
    }

    #[inline]
    pub fn from_word(word: u32) -> Self {
        Self {
            network_id: (word >> 16) as u16,
            node_id: (word >> 8) as u8,
            port_id: word as u8,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetStackSendError {
    SocketSend(SocketSendError),
    InterfaceSend(InterfaceSendError),
    NoRoute,
    AnyPortMissingKey,
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
    pub fn with_interface_manager<F: FnOnce(&mut M) -> U, U>(&'static self, f: F) -> U {
        self.inner.with_lock(|inner| f(&mut inner.manager))
    }

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
            None,
        )?;
        // TODO: assert seq nos match somewhere? do we NEED seq nos if we have
        // port ids now?
        let resp = resp_hdl.recv().await;
        Ok(resp.t)
    }

    pub fn send_raw(
        &'static self,
        src: Address,
        dst: Address,
        key: Option<Key>,
        body: &[u8],
        seq_no: Option<u16>,
    ) -> Result<(), NetStackSendError> {
        if dst.port_id == 0 && key.is_none() {
            return Err(NetStackSendError::AnyPortMissingKey);
        }
        let local_bypass = src.net_node_any() && dst.net_node_any();

        self.inner.with_lock(|inner| {
            let res = if !local_bypass {
                inner.manager.send_raw(src, dst, key, body)
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
                        if (port == dst.port_id)
                            || (dst.port_id == 0 && key.is_some_and(|k| k == skt_key))
                        {
                            let res = {
                                let f = vtable.send_raw;
                                let this: NonNull<SocketHeader> = socket;
                                let this: NonNull<()> = this.cast();
                                let seq_no = if let Some(seq) = seq_no {
                                    seq
                                } else {
                                    let seq = inner.seq_no;
                                    inner.seq_no = inner.seq_no.wrapping_add(1);
                                    seq
                                };

                                (f)(this, body, src, dst, seq_no)
                                    .map_err(NetStackSendError::SocketSend)
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

    pub fn send_ty<T: 'static + Serialize>(
        &'static self,
        src: Address,
        dst: Address,
        key: Key,
        t: T,
        seq_no: Option<u16>,
    ) -> Result<(), NetStackSendError> {
        // Can we assume the destination is local?
        let local_bypass = src.net_node_any() && dst.net_node_any();

        self.inner.with_lock(|inner| {
            let res = if !local_bypass {
                // Not local: offer to the interface manager to send
                inner.manager.send(src, dst, Some(key), &t)
            } else {
                // just skip to local sending
                Err(InterfaceSendError::DestinationLocal)
            };

            match res {
                // We sent it via the interface, all done. T is dropped naturally
                Ok(()) => Ok(()),
                Err(InterfaceSendError::DestinationLocal) => {
                    // Sending to a local interface means a potential move. Create a
                    // manuallydrop, if a send succeeds, then we have "moved from" here
                    // into the destination. If no send succeeds (e.g. no socket match
                    // or sending to the socket failed) then we will need to drop the
                    // value ourselves.
                    let mut t = ManuallyDrop::new(t);

                    // Check each socket to see if we want to send it there...
                    for socket in inner.sockets.iter_raw() {
                        let (port, vtable, skt_key) = unsafe {
                            let skt_ref = socket.as_ref();
                            let port = *skt_ref.port.get();
                            let vtable = skt_ref.vtable.clone();
                            (port, vtable, skt_ref.key)
                        };
                        // TODO: only allow port_id == 0 if there is only one matching port
                        // with this key.
                        if (port == dst.port_id || dst.port_id == 0) && key == skt_key {
                            let res = if let Some(f) = vtable.send_owned {
                                let this: NonNull<SocketHeader> = socket;
                                let this: NonNull<()> = this.cast();
                                let that: NonNull<ManuallyDrop<T>> = NonNull::from(&mut t);
                                let that: NonNull<()> = that.cast();
                                let seq_no = if let Some(seq) = seq_no {
                                    seq
                                } else {
                                    let seq = inner.seq_no;
                                    inner.seq_no = inner.seq_no.wrapping_add(1);
                                    seq
                                };
                                (f)(this, that, &TypeId::of::<T>(), src, dst, seq_no)
                                    .map_err(NetStackSendError::SocketSend)
                            } else if let Some(_f) = vtable.send_bor {
                                // TODO: if we support send borrowed, then we need to
                                // drop the manuallydrop here, success or failure.
                                todo!()
                            } else {
                                // todo: keep going? If we found the "right" destination and
                                // sending fails, then there's not much we can do. Probably: there
                                // is no case where a socket has NEITHER send_owned NOR send_bor,
                                // can we make this state impossible instead?
                                Err(NetStackSendError::SocketSend(SocketSendError::WhatTheHell))
                            };

                            // If sending failed, we did NOT move the T, which means it's on us
                            // to drop it.
                            if res.is_err() {
                                unsafe {
                                    ManuallyDrop::drop(&mut t);
                                }
                            }
                            return res;
                        }
                    }

                    // We reached the end of sockets. We need to drop this item.
                    unsafe {
                        ManuallyDrop::drop(&mut t);
                    }
                    Err(NetStackSendError::NoRoute)
                }
                Err(e) => Err(NetStackSendError::InterfaceSend(e)),
            }
        })
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
