//! Embassy-net UDP edge device profile.
//!
//! Thin wrapper that creates an [`EdgeFrameProcessor`] and delegates
//! to the generic [`RxTxWorker`](crate::interface_manager::transports::embassy_net_udp::RxTxWorker).

pub use crate::interface_manager::transports::embassy_net_udp::{
    RxTxError, RxTxWorker, SocketAlreadyActive, UDP_OVER_ETH_ERGOT_FRAME_SIZE_MAX,
    UDP_OVER_ETH_ERGOT_PAYLOAD_SIZE_MAX,
};
