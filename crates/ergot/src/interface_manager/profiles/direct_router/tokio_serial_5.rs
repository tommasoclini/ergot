//! A Tokio Serial based DirectRouter
//!
//! Thin wrapper around [`super::tokio_stream`] that handles serial port opening
//! and buffer clearing before delegating to the generic stream-based router.

use crate::interface_manager::LivenessConfig;
use crate::logging::warn;
use maitake_sync::WaitQueue;
use std::sync::Arc;
use tokio_serial_v5::{ClearBuffer, SerialPort, SerialPortBuilderExt};

use super::tokio_stream;
use crate::interface_manager::{
    interface_impls::tokio_stream::TokioStreamInterface, profiles::direct_router::DirectRouter,
};
use crate::net_stack::NetStackHandle;

#[derive(Debug, PartialEq)]
pub enum Error {
    OutOfNetIds,
    Serial(String),
}

pub async fn register_interface<N>(
    stack: N,
    serial_path: &str,
    baud: u32,
    max_ergot_packet_size: u16,
    outgoing_buffer_size: usize,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
) -> Result<u64, Error>
where
    N: NetStackHandle<Profile = DirectRouter<TokioStreamInterface>>,
    N: Send + 'static,
{
    let mut port = tokio_serial_v5::new(serial_path, baud)
        .open_native_async()
        .map_err(|e| Error::Serial(format!("Open Error: {:?}", e)))?;
    if let Err(e) = port.clear(ClearBuffer::All) {
        warn!("Failed to clear serial buffers: {:?}", e);
    }
    // Send a COBS frame boundary so the device can flush any stale
    // partial frame from a previous session and sync cleanly.
    let _ = std::io::Write::write_all(&mut port, &[0]);
    let (rx, tx) = tokio::io::split(port);

    tokio_stream::register_interface(
        stack,
        rx,
        tx,
        max_ergot_packet_size,
        outgoing_buffer_size,
        liveness,
        state_notify,
    )
    .await
    .map_err(|_| Error::OutOfNetIds)
}
