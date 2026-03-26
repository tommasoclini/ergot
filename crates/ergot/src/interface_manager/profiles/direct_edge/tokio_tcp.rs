//! A std+tcp edge device profile
//!
//! This is useful for std based devices/applications that can directly connect to a DirectRouter
//! using a tcp connection.

use std::sync::Arc;

use crate::{
    interface_manager::{
        InterfaceState, LivenessConfig, Profile,
        interface_impls::tokio_tcp::TokioTcpInterface,
        profiles::direct_edge::{DirectEdge, EdgeFrameProcessor},
        transports::tokio_cobs_stream::CobsStreamRxWorker,
    },
    logging::{error, info, trace, warn},
    net_stack::NetStackHandle,
};

use crate::interface_manager::utils::std::StdQueue;
use bbqueue::{prod_cons::stream::StreamConsumer, traits::bbqhdl::BbqHandle};
use maitake_sync::WaitQueue;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpStream, tcp::OwnedWriteHalf},
    select,
};

pub type StdTcpClientIm = DirectEdge<TokioTcpInterface>;

#[derive(Debug, PartialEq)]
pub struct SocketAlreadyActive;

pub async fn register_target_interface<N>(
    stack: N,
    socket: TcpStream,
    queue: StdQueue,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
) -> Result<(), SocketAlreadyActive>
where
    N: NetStackHandle<Profile = DirectEdge<TokioTcpInterface>>,
    N: Send + 'static,
{
    let (rx, tx) = socket.into_split();
    let closer = Arc::new(WaitQueue::new());
    stack.stack().manage_profile(|im| {
        match im.interface_state(()) {
            Some(InterfaceState::Down) => {}
            Some(InterfaceState::Inactive) => return Err(SocketAlreadyActive),
            Some(InterfaceState::ActiveLocal { .. }) => return Err(SocketAlreadyActive),
            Some(InterfaceState::Active { .. }) => return Err(SocketAlreadyActive),
            None => {}
        }

        im.set_interface_state((), InterfaceState::Inactive)
            .map_err(|_| SocketAlreadyActive)?;

        Ok(())
    })?;
    if let Some(notify) = &state_notify {
        notify.wake_all();
    }

    let notify_clone = state_notify.clone();
    let closer_clone = closer.clone();
    let stack_clone = stack.clone();

    let mut rx_worker = CobsStreamRxWorker {
        nsh: stack,
        reader: rx,
        closer: closer.clone(),
        processor: EdgeFrameProcessor::new(),
        ident: (),
        liveness,
        state_notify,
        cobs_buf_size: 1024 * 1024,
    };

    tokio::task::spawn(async move {
        let _res = rx_worker.run().await;
        stack_clone.stack().manage_profile(|im| {
            _ = im.set_interface_state((), InterfaceState::Down);
        });
        if let Some(notify) = &notify_clone {
            notify.wake_all();
        }
    });
    tokio::task::spawn(tx_worker(tx, queue.stream_consumer(), closer_clone));
    Ok(())
}

async fn tx_worker(mut tx: OwnedWriteHalf, rx: StreamConsumer<StdQueue>, closer: Arc<WaitQueue>) {
    info!("Started tx_worker");
    loop {
        let rxf = rx.wait_read();
        let clf = closer.wait();

        let frame = select! {
            r = rxf => r,
            _c = clf => {
                break;
            }
        };

        let len = frame.len();
        trace!("sending pkt len:{}", len);
        let res = tx.write_all(&frame).await;
        frame.release(len);
        if let Err(e) = res {
            error!("Tx Error. socket: {:?}, error: {:?}", tx, e);
            break;
        }
    }
    warn!("Closing interface");
}
