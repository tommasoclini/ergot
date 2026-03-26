//! An NUSB based DirectRouter
//!
//! This implementation can be used to connect to a number of direct edge USB devices.

use crate::logging::{debug, error, info, warn};
use bbqueue::{prod_cons::framed::FramedConsumer, traits::bbqhdl::BbqHandle};
use maitake_sync::WaitQueue;
use nusb::transfer::Queue;
use std::sync::Arc;
use tokio::select;

use crate::{
    interface_manager::{
        InterfaceState, Profile,
        interface_impls::nusb_bulk::{NewDevice, NusbBulk},
        profiles::direct_router::{DirectRouter, RouterFrameProcessor},
        transports::nusb::NusbRxWorker,
        utils::{
            framed_stream::Sink,
            std::{StdQueue, new_std_queue},
        },
    },
    net_stack::NetStackHandle,
};

#[derive(Debug, PartialEq)]
pub enum Error {
    OutOfNetIds,
}

struct TxWorker {
    net_id: u16,
    boq: Queue<Vec<u8>>,
    rx: FramedConsumer<StdQueue>,
    closer: Arc<WaitQueue>,
    max_usb_frame_size: Option<usize>,
}

impl TxWorker {
    async fn run(mut self) {
        self.run_inner().await;
        warn!("Closing interface {}", self.net_id);
        self.closer.close();
    }

    async fn run_inner(&mut self) {
        info!("Started tx_worker for net_id {}", self.net_id);
        loop {
            let rxf = self.rx.wait_read();
            let clf = self.closer.wait();

            let frame = select! {
                r = rxf => r,
                _c = clf => {
                    return;
                }
            };

            let len = frame.len();
            debug!("sending pkt len:{} on net_id {}", len, self.net_id);

            let needs_zlp = if let Some(mps) = &self.max_usb_frame_size {
                (len % mps) == 0
            } else {
                true
            };

            self.boq.submit(frame.to_vec());

            if needs_zlp {
                self.boq.submit(vec![]);
            }

            let send_res = self.boq.next_complete().await;
            if let Err(e) = send_res.status {
                error!("Output Queue Error: {:?}", e);
                return;
            }

            if needs_zlp {
                let send_res = self.boq.next_complete().await;
                if let Err(e) = send_res.status {
                    error!("Output Queue Error: {:?}", e);
                    return;
                }
            }

            frame.release();
        }
    }
}

pub async fn register_interface<N>(
    stack: N,
    device: NewDevice,
    max_ergot_packet_size: u16,
    outgoing_buffer_size: usize,
    state_notify: Option<Arc<WaitQueue>>,
) -> Result<u64, Error>
where
    N: NetStackHandle<Profile = DirectRouter<NusbBulk>>,
    N: Send + 'static,
{
    let q: StdQueue = new_std_queue(outgoing_buffer_size);
    let res = stack.stack().manage_profile(|im| {
        let ident =
            im.register_interface(Sink::new_from_handle(q.clone(), max_ergot_packet_size))?;
        let state = im.interface_state(ident)?;
        match state {
            InterfaceState::Active { net_id, node_id: _ } => Some((ident, net_id)),
            _ => {
                _ = im.deregister_interface(ident);
                None
            }
        }
    });
    let Some((ident, net_id)) = res else {
        return Err(Error::OutOfNetIds);
    };
    let closer = Arc::new(WaitQueue::new());

    let notify_clone = state_notify.clone();
    let nsh_clone = stack.clone();

    let mut rx_worker = NusbRxWorker {
        nsh: stack.clone(),
        biq: device.biq,
        closer: closer.clone(),
        processor: RouterFrameProcessor::new(net_id),
        ident,
        mtu: max_ergot_packet_size,
        state_notify,
    };

    let tx_worker = TxWorker {
        net_id,
        rx: <StdQueue as BbqHandle>::framed_consumer(&q),
        closer: closer.clone(),
        boq: device.boq,
        max_usb_frame_size: device.max_packet_size,
    };

    stack.stack().manage_profile(|im| {
        im.set_interface_closer(ident, closer.clone());
    });

    tokio::task::spawn(async move {
        let close = rx_worker.closer.clone();
        select! {
            _run = rx_worker.run() => {
                close.close();
            },
            _clf = close.wait() => {},
        }
        nsh_clone.stack().manage_profile(|im| {
            _ = im.deregister_interface(ident);
        });
        if let Some(notify) = &notify_clone {
            notify.wake_all();
        }
    });
    tokio::task::spawn(tx_worker.run());

    Ok(ident)
}
