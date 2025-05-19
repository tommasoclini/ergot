use std::ptr::NonNull;

use cordyceps::List;
use mutex::{BlockingMutex, ConstInit, ScopedRawMutex};
use socket::SocketHeader;

pub mod socket;

struct NetStackInner {
    sockets: List<SocketHeader>,
    port_ctr: u8,
}

impl NetStackInner {
    pub const fn new() -> Self {
        Self {
            sockets: List::new(),
            port_ctr: 0,
        }
    }
}

impl Default for NetStackInner {
    fn default() -> Self {
        Self::new()
    }
}

pub struct NetStack<R: ScopedRawMutex> {
    inner: BlockingMutex<R, NetStackInner>,
}

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
}

impl<R> NetStack<R>
where
    R: ScopedRawMutex + ConstInit,
{
    pub const fn new() -> Self {
        Self {
            inner: BlockingMutex::new(NetStackInner::new()),
        }
    }
}

impl<R> NetStack<R>
where
    R: ScopedRawMutex,
{
    pub(crate) unsafe fn attach_socket(&'static self, node: NonNull<SocketHeader>) {
        self.inner.with_lock(|inner| {
            // TODO: smarter than this, do something like littlefs2's "next free block"
            // bitmap thing?
            let start = inner.port_ctr;
            loop {
                inner.port_ctr = inner.port_ctr.wrapping_add(1).max(1);
                let exists = inner
                    .sockets
                    .iter()
                    .any(|s| unsafe { *s.port.get() } == inner.port_ctr);
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
        })
    }
}

impl<R> Default for NetStack<R>
where
    R: ScopedRawMutex + ConstInit,
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
