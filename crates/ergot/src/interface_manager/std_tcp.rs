/*
    Let's see, we're going to need:

    * Some kind of hashmap/vec of active interfaces, by network id?
        * IF we use a vec, we should NOT use the index as the ID, it may be sparse
    * The actual interface type probably gets defined by the interface manager
    * The interface which follows the pinned rules, and removes itself on drop
    * THIS version of the routing interface probably will not allow for other routers,
        we probably have to assume we are the only one assigning network IDs until a
        later point
    * The associated "simple" version of a client probably needs a stub routing interface
        that picks up the network ID from the destination address
    * Honestly we might want to have an `Arc` version of the netstack, or we need some kind
        of Once construction.
    * The interface manager needs some kind of "handle" construction so that we can get mut
        access to it, or we need an accessor via the netstack
*/

use std::sync::Arc;
use std::{cell::UnsafeCell, mem::MaybeUninit};

use crate::{Address, NetStack};
use acc::{CobsAccumulator, FeedResult};
use maitake_sync::WaitQueue;
use mutex::ScopedRawMutex;
use postcard_rpc::Key;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio::{
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    select,
    sync::mpsc::error::TrySendError,
};

use super::{ConstInit, InterfaceManager, InterfaceSendError};

pub struct StdTcpIm {
    init: bool,
    inner: UnsafeCell<MaybeUninit<StdTcpImInner>>,
}

unsafe impl Sync for StdTcpIm {}

#[derive(Default)]
pub struct StdTcpImInner {
    // TODO: we probably want something like iddqd for a hashset sorted by
    // net_id, as well as a list of "allocated" netids, mapped to the
    // interface they are associated with
    interfaces: Vec<StdTcpInterface>,
    seq_no: u16,
    any_closed: bool,
}

struct OwnedFrame {
    src: Address,
    dst: Address,
    seq: u16,
    key: Option<Key>,
    body: Vec<u8>,
}

fn ser_frame(frame: OwnedFrame) -> Vec<u8> {
    let dst_any = frame.dst.port_id == 0;
    let src = frame.src.as_u32();
    let dst = frame.dst.as_u32();
    let seq = frame.seq;

    let mut out = vec![];
    // TODO: This is bad and does a ton of allocs. yolo
    //
    out.extend_from_slice(&postcard::to_stdvec(&src).unwrap());
    out.extend_from_slice(&postcard::to_stdvec(&dst).unwrap());
    if dst_any {
        let key = frame.key.unwrap();
        out.extend_from_slice(&postcard::to_stdvec(&key).unwrap());
    }

    out.extend_from_slice(&postcard::to_stdvec(&seq).unwrap());
    out.extend_from_slice(&frame.body);
    cobs::encode_vec(&out)
}

fn de_frame(remain: &[u8]) -> Option<OwnedFrame> {
    let (src_word, remain) = postcard::take_from_bytes::<u32>(remain).ok()?;
    let src = Address::from_word(src_word);
    let (dst_word, remain) = postcard::take_from_bytes::<u32>(remain).ok()?;
    let dst = Address::from_word(dst_word);
    let (key, remain) = if dst.port_id == 0 {
        let (k, r) = postcard::take_from_bytes::<Key>(remain).ok()?;
        (Some(k), r)
    } else {
        (None, remain)
    };

    let (seq, remain) = postcard::take_from_bytes::<u16>(remain).ok()?;
    let body = remain.to_vec();

    Some(OwnedFrame {
        src,
        dst,
        seq,
        key,
        body,
    })
}

async fn tx_worker(
    net_id: u16,
    mut tx: OwnedWriteHalf,
    mut rx: Receiver<OwnedFrame>,
    closer: Arc<WaitQueue>,
) {
    println!("Started tx_worker for net_id {net_id}");
    loop {
        let rxf = rx.recv();
        let clf = closer.wait();

        let frame = select! {
            r = rxf => {
                if let Some(frame) = r {
                    frame
                } else {
                    println!("tx_worker {net_id} rx closed!");
                    closer.close();
                    break;
                }
            }
            _c = clf => {
                break;
            }
        };

        let msg = ser_frame(frame);
        println!("sending pkt len:{} on net_id {net_id}", msg.len());
        let res = tx.write_all(&msg).await;
        if let Err(e) = res {
            println!("Err: {e:?}");
            break;
        }
    }
    // TODO: GC waker?
    println!("Closing interface {net_id}");
}

impl StdTcpImInner {
    pub fn alloc_intfc(&mut self, tx: OwnedWriteHalf) -> Option<(u16, Arc<WaitQueue>)> {
        let closer = Arc::new(WaitQueue::new());
        if self.interfaces.is_empty() {
            // todo: configurable channel depth
            let (ctx, crx) = channel(64);
            let net_id = 1;
            tokio::task::spawn(tx_worker(net_id, tx, crx, closer.clone()));
            self.interfaces.push(StdTcpInterface {
                net_id,
                skt_tx: ctx,
                closer: closer.clone(),
            });
            println!("Alloc'd net_id 1");
            return Some((net_id, closer));
        } else if self.interfaces.len() >= 65534 {
            println!("Out of netids!");
            return None;
        }

        // If we closed any interfaces, then collect
        if self.any_closed {
            self.interfaces.retain(|int| {
                let closed = int.closer.is_closed();
                if closed {
                    println!("Collecting interface {}", int.net_id);
                }
                !closed
            });
        }

        let mut net_id = 1;
        // we're not empty, find the lowest free address by counting the
        // indexes, and if we find a discontinuity, allocate the first one.
        for intfc in self.interfaces.iter() {
            if intfc.net_id > net_id {
                println!("Found gap: {net_id}");
                break;
            }
            debug_assert!(intfc.net_id == net_id);
            net_id += 1;
        }
        // EITHER: We've found a gap that we can use, OR we've iterated all
        // interfaces, which means that we had contiguous allocations but we
        // have not exhausted the range.
        debug_assert!(net_id > 0 && net_id != u16::MAX);
        let (ctx, crx) = channel(64);
        println!("allocated net_id {net_id}");

        tokio::task::spawn(tx_worker(net_id, tx, crx, closer.clone()));
        self.interfaces.push(StdTcpInterface {
            net_id,
            skt_tx: ctx,
            closer: closer.clone(),
        });
        self.interfaces.sort_unstable_by_key(|i| i.net_id);
        Some((net_id, closer))
    }
}

pub struct StdTcpInterface {
    net_id: u16,
    skt_tx: Sender<OwnedFrame>,
    closer: Arc<WaitQueue>,
}

impl StdTcpIm {
    const fn new() -> Self {
        Self {
            init: false,
            inner: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    fn get_or_init_inner(&mut self) -> &mut StdTcpImInner {
        let inner = self.inner.get_mut();
        if self.init {
            unsafe { inner.assume_init_mut() }
        } else {
            let imr = inner.write(StdTcpImInner::default());
            self.init = true;
            imr
        }
    }
}

impl Default for StdTcpIm {
    fn default() -> Self {
        Self::new()
    }
}

impl ConstInit for StdTcpIm {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self::new();
}

impl InterfaceManager for StdTcpIm {
    fn send<T: serde::Serialize>(
        &mut self,
        mut src: crate::Address,
        dst: crate::Address,
        key: Option<Key>,
        data: &T,
    ) -> Result<(), InterfaceSendError> {
        // todo: make this state impossible? enum of dst w/ or w/o key?
        assert!(!(dst.port_id == 0 && key.is_none()));

        let inner = self.get_or_init_inner();
        // todo: we only handle direct dests, we will probably also want to search
        // some kind of net_id:interface routing table
        let Ok(idx) = inner
            .interfaces
            .binary_search_by_key(&dst.network_id, |int| int.net_id)
        else {
            return Err(InterfaceSendError::NoRouteToDest);
        };

        let interface = &inner.interfaces[idx];
        // TODO: Assumption: "we" are always node_id==1
        if dst.network_id == interface.net_id && dst.node_id == 1 {
            return Err(InterfaceSendError::DestinationLocal);
        }
        // If the source is local, rewrite the source using this interface's
        // information so responses can find their way back here
        if src.net_node_any() {
            // todo: if we know the destination is EXACTLY this network,
            // we could leave the network_id local to allow for shorter
            // addresses
            src.network_id = interface.net_id;
            src.node_id = 1;
        }

        let seq_no = inner.seq_no;
        inner.seq_no = inner.seq_no.wrapping_add(1);
        let res = interface.skt_tx.try_send(OwnedFrame {
            src,
            dst,
            seq: seq_no,
            key,
            body: postcard::to_stdvec(data).unwrap(),
        });
        match res {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(InterfaceSendError::InterfaceFull),
            Err(TrySendError::Closed(_)) => {
                let rem = inner.interfaces.remove(idx);
                rem.closer.close();
                Err(InterfaceSendError::NoRouteToDest)
            }
        }
    }

    fn send_raw(
        &mut self,
        mut src: crate::Address,
        dst: crate::Address,
        key: Option<Key>,
        data: &[u8],
    ) -> Result<(), InterfaceSendError> {
        // todo: make this state impossible? enum of dst w/ or w/o key?
        assert!(!(dst.port_id == 0 && key.is_none()));

        let inner = self.get_or_init_inner();
        // todo: dedupe w/ send
        //
        // todo: we only handle direct dests
        let Ok(idx) = inner
            .interfaces
            .binary_search_by_key(&dst.network_id, |int| int.net_id)
        else {
            return Err(InterfaceSendError::NoRouteToDest);
        };

        let interface = &inner.interfaces[idx];
        // TODO: Assumption: "we" are always node_id==1
        if dst.network_id == interface.net_id && dst.node_id == 1 {
            return Err(InterfaceSendError::DestinationLocal);
        }
        // If the source is local, rewrite the source using this interface's
        // information so responses can find their way back here
        if src.net_node_any() {
            // todo: if we know the destination is EXACTLY this network,
            // we could leave the network_id local to allow for shorter
            // addresses
            src.network_id = interface.net_id;
            src.node_id = 1;
        }

        let seq_no = inner.seq_no;
        inner.seq_no = inner.seq_no.wrapping_add(1);
        let res = interface.skt_tx.try_send(OwnedFrame {
            src,
            dst,
            seq: seq_no,
            key,
            body: data.to_vec(),
        });
        match res {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(InterfaceSendError::InterfaceFull),
            Err(TrySendError::Closed(_)) => {
                inner.interfaces.remove(idx);
                Err(InterfaceSendError::NoRouteToDest)
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Error {
    OutOfNetIds,
}

pub struct StdTcpRecvHdl<R: ScopedRawMutex + 'static> {
    stack: &'static NetStack<R, StdTcpIm>,
    // TODO: when we have more real networking and we could possibly
    // have conflicting net_id assignments, we might need to have a
    // shared ref to an Arc<AtomicU16> or something for net_id?
    //
    // for now, stdtcp assumes it is the only "seed" router, meaning that
    // it is solely in charge of assigning netids
    net_id: u16,
    skt: OwnedReadHalf,
    closer: Arc<WaitQueue>,
}

#[derive(Debug, PartialEq)]
pub enum ReceiverError {
    SocketClosed,
}

impl<R: ScopedRawMutex + 'static> StdTcpRecvHdl<R> {
    pub async fn run(mut self) -> Result<(), ReceiverError> {
        let res = self.run_inner().await;
        self.closer.close();
        // todo: this could live somewhere else?
        self.stack.with_interface_manager(|im| {
            let inner = im.get_or_init_inner();
            inner.any_closed = true;
        });
        res
    }

    pub async fn run_inner(&mut self) -> Result<(), ReceiverError> {
        let mut cobs_buf = CobsAccumulator::new(1024 * 1024);
        let mut raw_buf = [0u8; 4096];

        loop {
            let rd = self.skt.read(&mut raw_buf);
            let close = self.closer.wait();

            let ct = select! {
                r = rd => {
                    match r {
                        Ok(0) | Err(_) => {
                            println!("recv run {} closed", self.net_id);
                            return Err(ReceiverError::SocketClosed)
                        },
                        Ok(ct) => ct,
                    }
                }
                _c = close => {
                    return Err(ReceiverError::SocketClosed);
                }
            };

            let buf = &raw_buf[..ct];
            let mut window = buf;

            'cobs: while !window.is_empty() {
                window = match cobs_buf.feed_raw(window) {
                    FeedResult::Consumed => break 'cobs,
                    FeedResult::OverFull(new_wind) => new_wind,
                    FeedResult::DeserError(new_wind) => new_wind,
                    FeedResult::Success { data, remaining } => {
                        // Successfully de-cobs'd a packet, now we need to
                        // do something with it.
                        if let Some(frame) = de_frame(data) {
                            let res =
                                self.stack
                                    .send_raw(frame.src, frame.dst, frame.key, &frame.body, Some(frame.seq));
                            match res {
                                Ok(()) => {}
                                Err(e) => {
                                    // TODO: match on error, potentially try to send NAK?
                                    panic!("recv->send error: {e:?}");
                                }
                            }
                        } else {
                            println!("Decode error! Ignoring frame on net_id {}", self.net_id);
                        }

                        remaining
                    }
                };
            }
        }
    }
}

pub fn register_interface<R: ScopedRawMutex>(
    stack: &'static NetStack<R, StdTcpIm>,
    socket: TcpStream,
) -> Result<StdTcpRecvHdl<R>, Error> {
    let (rx, tx) = socket.into_split();
    stack.with_interface_manager(|im| {
        let inner = im.get_or_init_inner();
        if let Some((addr, closer)) = inner.alloc_intfc(tx) {
            Ok(StdTcpRecvHdl {
                stack,
                net_id: addr,
                skt: rx,
                closer,
            })
        } else {
            Err(Error::OutOfNetIds)
        }
    })
}

mod acc {
    pub struct CobsAccumulator {
        buf: Box<[u8]>,
        idx: usize,
    }

    /// The result of feeding the accumulator.
    pub enum FeedResult<'input, 'buf> {
        /// Consumed all data, still pending.
        Consumed,

        /// Buffer was filled. Contains remaining section of input, if any.
        OverFull(&'input [u8]),

        /// Reached end of chunk, but deserialization failed. Contains remaining section of input, if.
        /// any
        DeserError(&'input [u8]),

        Success {
            /// Decoded data.
            data: &'buf [u8],

            /// Remaining data left in the buffer after deserializing.
            remaining: &'input [u8],
        },
    }

    impl CobsAccumulator {
        /// Create a new accumulator.
        pub fn new(sz: usize) -> Self {
            CobsAccumulator {
                buf: vec![0u8; sz].into_boxed_slice(),
                idx: 0,
            }
        }

        /// Appends data to the internal buffer and attempts to deserialize the accumulated data into
        /// `T`.
        ///
        /// This differs from feed, as it allows the `T` to reference data within the internal buffer, but
        /// mutably borrows the accumulator for the lifetime of the deserialization.
        /// If `T` does not require the reference, the borrow of `self` ends at the end of the function.
        pub fn feed_raw<'me, 'input>(
            &'me mut self,
            input: &'input [u8],
        ) -> FeedResult<'input, 'me> {
            if input.is_empty() {
                return FeedResult::Consumed;
            }

            let zero_pos = input.iter().position(|&i| i == 0);
            let max_len = self.buf.len();

            if let Some(n) = zero_pos {
                // Yes! We have an end of message here.
                // Add one to include the zero in the "take" portion
                // of the buffer, rather than in "release".
                let (take, release) = input.split_at(n + 1);

                // TODO(AJM): We could special case when idx == 0 to avoid copying
                // into the dest buffer if there's a whole packet in the input

                // Does it fit?
                if (self.idx + take.len()) <= max_len {
                    // Aw yiss - add to array
                    self.extend_unchecked(take);

                    let retval = match cobs::decode_in_place(&mut self.buf[..self.idx]) {
                        Ok(ct) => FeedResult::Success {
                            data: &self.buf[..ct],
                            remaining: release,
                        },
                        Err(_) => FeedResult::DeserError(release),
                    };
                    self.idx = 0;
                    retval
                } else {
                    self.idx = 0;
                    FeedResult::OverFull(release)
                }
            } else {
                // Does it fit?
                if (self.idx + input.len()) > max_len {
                    // nope
                    let new_start = max_len - self.idx;
                    self.idx = 0;
                    FeedResult::OverFull(&input[new_start..])
                } else {
                    // yup!
                    self.extend_unchecked(input);
                    FeedResult::Consumed
                }
            }
        }

        /// Extend the internal buffer with the given input.
        ///
        /// # Panics
        ///
        /// Will panic if the input does not fit in the internal buffer.
        fn extend_unchecked(&mut self, input: &[u8]) {
            let new_end = self.idx + input.len();
            self.buf[self.idx..new_end].copy_from_slice(input);
            self.idx = new_end;
        }
    }
}
