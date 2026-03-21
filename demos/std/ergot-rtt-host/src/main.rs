//! Host-side ergot router using RTT transport via probe-rs
//!
//! This demo shows how to connect to an embedded device via RTT (Real-Time Transfer)
//! using a debug probe (ST-Link, J-Link, etc.) and communicate using the ergot protocol.
//!
//! Optionally decodes defmt frames from a separate RTT channel or from
//! the ergot network (ergot/.well-known/defmt topic).
//!
//! Usage:
//!   ergot-rtt-host --chip STM32G431KBTx --up-channel 1 --down-channel 0
//!   ergot-rtt-host --chip STM32G431KBTx --elf firmware.elf --defmt-channel 0
//!   ergot-rtt-host --chip STM32G431KBTx --elf firmware.elf --defmt-via-ergot

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use clap::Parser;
use ergot::{
    Address,
    toolkits::tokio_stream::{self as stream_kit, EdgeStack},
    well_known::ErgotPingEndpoint,
};
use futures::SinkExt;
use log::{error, info, warn};
use probe_rs::{
    Permissions,
    probe::list::Lister,
    rtt::{Rtt, ScanRegion},
};
use std::io;
use std::time::Duration;
use tokio::time::{interval, timeout};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::{CopyToBytes, SinkWriter, StreamReader};
use tokio_util::sync::PollSender;

/// Host-side ergot router using RTT transport
#[derive(Parser, Debug)]
#[command(name = "ergot-rtt-host")]
#[command(about = "Connect to embedded devices via RTT and route ergot messages")]
struct Args {
    /// Target chip name (e.g., STM32G431KBTx, nRF52840_xxAA)
    #[arg(short, long)]
    chip: String,

    /// Probe selector in format VID:PID or VID:PID:SERIAL
    #[arg(short, long)]
    probe: Option<String>,

    /// RTT up channel index for ergot data (device -> host)
    #[arg(long, default_value = "1")]
    up_channel: usize,

    /// RTT down channel index for ergot data (host -> device)
    #[arg(long, default_value = "0")]
    down_channel: usize,

    /// Path to device ELF file for defmt decoding
    #[arg(long)]
    elf: Option<PathBuf>,

    /// RTT up channel index for defmt data (default: 0, disabled if no --elf)
    #[arg(long, default_value = "0")]
    defmt_channel: usize,

    /// Receive defmt frames via ergot network topic instead of raw RTT channel
    #[arg(long)]
    defmt_via_ergot: bool,
}

const ERGOT_MTU: u16 = 512;

fn connect_probe(probe_selector: Option<&str>, chip: &str) -> Result<probe_rs::Session> {
    info!("Connecting to chip: {}", chip);

    let lister = Lister::new();

    let probe = if let Some(selector) = probe_selector {
        info!("Using probe selector: {}", selector);
        let probes = lister.list_all();

        let parts: Vec<&str> = selector.split(':').collect();
        let (vid, pid, serial) = match parts.len() {
            2 => {
                let vid = u16::from_str_radix(parts[0], 16)
                    .with_context(|| format!("Invalid VID: {}", parts[0]))?;
                let pid = u16::from_str_radix(parts[1], 16)
                    .with_context(|| format!("Invalid PID: {}", parts[1]))?;
                (vid, pid, None)
            }
            3 => {
                let vid = u16::from_str_radix(parts[0], 16)
                    .with_context(|| format!("Invalid VID: {}", parts[0]))?;
                let pid = u16::from_str_radix(parts[1], 16)
                    .with_context(|| format!("Invalid PID: {}", parts[1]))?;
                (vid, pid, Some(parts[2]))
            }
            _ => {
                return Err(anyhow!(
                    "Invalid probe selector. Use VID:PID or VID:PID:SERIAL"
                ));
            }
        };

        let probe_info = probes
            .into_iter()
            .find(|p| {
                p.vendor_id == vid
                    && p.product_id == pid
                    && (serial.is_none() || p.serial_number.as_deref() == serial)
            })
            .ok_or_else(|| anyhow!("No matching probe found for: {}", selector))?;

        probe_info.open().context("Failed to open probe")?
    } else {
        let probes = lister.list_all();
        if probes.is_empty() {
            return Err(anyhow!("No debug probes found"));
        }
        info!("Auto-selecting first probe: {:?}", probes[0]);
        probes[0].open().context("Failed to open probe")?
    };

    let session = probe
        .attach(chip, Permissions::default())
        .context("Failed to attach to target")?;

    info!("Attached to target");
    Ok(session)
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    // Validate that ergot and defmt don't share the same RTT channel
    if !args.defmt_via_ergot && args.elf.is_some() && args.defmt_channel == args.up_channel {
        return Err(anyhow!(
            "defmt channel ({}) and ergot up channel ({}) must be different — \
             reading the same RTT channel for both would corrupt both streams. \
             Use --defmt-via-ergot to receive defmt frames over the ergot network instead.",
            args.defmt_channel,
            args.up_channel
        ));
    }

    let mut session = connect_probe(args.probe.as_deref(), &args.chip)?;

    let has_rtt_defmt = args.elf.is_some() && !args.defmt_via_ergot;
    let has_ergot_defmt = args.elf.is_some() && args.defmt_via_ergot;

    // Create ergot stack using the tokio_stream toolkit
    let queue = stream_kit::new_std_queue(4096);
    let stack: EdgeStack = stream_kit::new_controller_stack(&queue, ERGOT_MTU);

    info!("Ergot RTT host started");

    // Set up channels between blocking RTT thread and async world.
    //
    // RX (device→host): RTT thread sends bytes via blocking_send,
    //   StreamReader wraps the receiver into AsyncRead for ergot.
    // TX (host→device): ergot writes via SinkWriter→PollSender→channel,
    //   RTT thread receives with try_recv.
    let ergot_up_ch = args.up_channel;
    let ergot_down_ch = args.down_channel;
    let defmt_ch = args.defmt_channel;

    // RX channel: RTT thread → ergot (AsyncRead via StreamReader)
    let (ergot_rx_tx, ergot_rx_rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(64);
    let reader = StreamReader::new(ReceiverStream::new(ergot_rx_rx));

    // TX channel: ergot (AsyncWrite via SinkWriter) → RTT thread
    let (ergot_tx_tx, mut ergot_tx_rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let writer = SinkWriter::new(CopyToBytes::new(
        PollSender::new(ergot_tx_tx).sink_map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e)),
    ));

    let (defmt_data_tx, defmt_data_rx) = if has_rtt_defmt {
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // RTT I/O thread — owns Session, Core and Rtt, never re-attaches
    std::thread::spawn(move || {
        let mut core = session.core(0).expect("Failed to get core 0");
        let mut rtt =
            Rtt::attach_region(&mut core, &ScanRegion::Ram).expect("Failed to attach to RTT");

        info!("RTT attached successfully");
        for (idx, channel) in rtt.up_channels().iter().enumerate() {
            info!(
                "RTT up channel {}: {} (size: {})",
                idx,
                channel.name().unwrap_or("unnamed"),
                channel.buffer_size()
            );
        }
        for (idx, channel) in rtt.down_channels().iter().enumerate() {
            info!(
                "RTT down channel {}: {} (size: {})",
                idx,
                channel.name().unwrap_or("unnamed"),
                channel.buffer_size()
            );
        }

        let mut ergot_rx_buf = [0u8; 2048];
        let mut defmt_buf = [0u8; 4096];

        loop {
            let mut did_work = false;

            // 1. Read ergot data from device → send to async world
            if let Some(channel) = rtt.up_channel(ergot_up_ch) {
                match channel.read(&mut core, &mut ergot_rx_buf) {
                    Ok(n) if n > 0 => {
                        did_work = true;
                        let _ = ergot_rx_tx
                            .blocking_send(Ok(Bytes::copy_from_slice(&ergot_rx_buf[..n])));
                    }
                    Ok(_) => {}
                    Err(e) => error!("RTT ergot read error: {}", e),
                }
            }

            // 2. Write ergot data to device ← receive from async world
            while let Ok(data) = ergot_tx_rx.try_recv() {
                if let Some(channel) = rtt.down_channel(ergot_down_ch) {
                    if let Err(e) = channel.write(&mut core, &data) {
                        error!("RTT ergot write error: {}", e);
                    }
                    did_work = true;
                }
            }

            // 3. Read defmt data from device (only when using raw RTT defmt)
            if let Some(ref defmt_tx) = defmt_data_tx {
                match rtt.up_channel(defmt_ch) {
                    Some(channel) => match channel.read(&mut core, &mut defmt_buf) {
                        Ok(n) if n > 0 => {
                            did_work = true;
                            info!("defmt: read {} bytes", n);
                            let _ = defmt_tx.blocking_send(defmt_buf[..n].to_vec());
                        }
                        Ok(_) => {}
                        Err(e) => error!("RTT defmt read error: {}", e),
                    },
                    None => {
                        error!("defmt channel {} not available", defmt_ch);
                    }
                }
            }

            if !did_work {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });

    // Register with ergot's tokio_stream toolkit — handles all COBS framing
    stream_kit::register_controller_stream(stack.clone(), reader, writer, queue)
        .await
        .map_err(|_| anyhow!("Interface already active"))?;

    // defmt decoder task (raw RTT channel)
    if let (Some(elf_path), Some(mut defmt_data_rx)) = (&args.elf, defmt_data_rx)
        && !args.defmt_via_ergot
    {
        let elf_bytes = std::fs::read(elf_path)?;
        let table = defmt_decoder::Table::parse(&elf_bytes)
            .context("Failed to parse defmt table")?
            .ok_or_else(|| anyhow!("No .defmt section"))?;

        tokio::spawn(async move {
            let mut stream_decoder = table.new_stream_decoder();

            while let Some(data) = defmt_data_rx.recv().await {
                stream_decoder.received(&data);
                loop {
                    match stream_decoder.decode() {
                        Ok(frame) => {
                            println!("[defmt] {}", frame.display(true));
                        }
                        Err(defmt_decoder::DecodeError::UnexpectedEof) => break,
                        Err(defmt_decoder::DecodeError::Malformed) => {
                            warn!("Malformed defmt frame");
                            break;
                        }
                    }
                }
            }
        });
    }

    // defmt-via-ergot decoder task
    if has_ergot_defmt {
        let elf_bytes = std::fs::read(args.elf.as_ref().unwrap())?;
        let table = defmt_decoder::Table::parse(&elf_bytes)
            .context("Failed to parse defmt table")?
            .ok_or_else(|| anyhow!("No .defmt section"))?;

        info!("defmt-via-ergot decoder enabled");

        tokio::spawn({
            let stack = stack.clone();
            async move {
                use ergot::well_known::ErgotDefmtRxOwnedTopic;
                use std::pin::pin;

                let sub = stack
                    .topics()
                    .heap_bounded_receiver::<ErgotDefmtRxOwnedTopic>(64, None);
                let sub = pin!(sub);
                let mut hdl = sub.subscribe();
                let mut stream_decoder = table.new_stream_decoder();

                info!("Waiting for defmt frames on ergot topic...");
                loop {
                    let msg = hdl.recv().await;
                    info!(
                        "Got defmt frame via ergot: {} bytes from {}.{}",
                        msg.t.frame.len(),
                        msg.hdr.src.network_id,
                        msg.hdr.src.node_id
                    );
                    stream_decoder.received(&msg.t.frame);
                    loop {
                        match stream_decoder.decode() {
                            Ok(frame) => {
                                println!("[defmt-ergot] {}", frame.display(true));
                            }
                            Err(defmt_decoder::DecodeError::UnexpectedEof) => break,
                            Err(defmt_decoder::DecodeError::Malformed) => {
                                warn!("Malformed defmt frame from ergot");
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    // Spawn log handler
    tokio::spawn({
        let stack = stack.clone();
        async move { stack.services().log_handler(64).await }
    });

    // Spawn ping task
    tokio::spawn({
        let stack = stack.clone();
        async move {
            let mut ival = interval(Duration::from_secs(5));
            let mut counter = 0u32;

            tokio::time::sleep(Duration::from_secs(2)).await;

            loop {
                ival.tick().await;
                let ping_val = counter;
                counter = counter.wrapping_add(1);

                let device_addr = Address {
                    network_id: 1,
                    node_id: 2,
                    port_id: 0,
                };

                let result = timeout(
                    Duration::from_millis(500),
                    stack
                        .endpoints()
                        .request::<ErgotPingEndpoint>(device_addr, &ping_val, None),
                )
                .await;

                match result {
                    Ok(Ok(response)) if response == ping_val => {
                        info!("Ping OK: {}", ping_val);
                    }
                    Ok(Ok(response)) => {
                        warn!("Ping mismatch: got {} expected {}", response, ping_val);
                    }
                    Ok(Err(e)) => {
                        warn!("Ping error: {:?}", e);
                    }
                    Err(_) => {
                        warn!("Ping timeout");
                    }
                }
            }
        }
    });

    info!("Press Ctrl+C to exit");
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
