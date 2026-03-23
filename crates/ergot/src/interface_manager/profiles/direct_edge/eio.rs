use cobs_acc::{CobsAccumulator, FeedResult};

use crate::eio::Read;

#[cfg(feature = "embassy-time")]
use crate::interface_manager::LivenessConfig;
use crate::interface_manager::profiles::direct_edge::CENTRAL_NODE_ID;
use crate::{
    interface_manager::{
        InterfaceState, Profile,
        interface_impls::embedded_io::IoInterface,
        profiles::direct_edge::{DirectEdge, process_frame},
    },
    net_stack::NetStackHandle,
};
#[cfg(feature = "embassy-time")]
use maitake_sync::WaitQueue;

pub type EmbeddedIoManager<Q> = DirectEdge<IoInterface<Q>>;

pub struct RxWorker<N, R>
where
    N: NetStackHandle,
    R: Read,
{
    nsh: N,
    rx: R,
    net_id: Option<u16>,
    ident: <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent,
    is_controller: bool,
    #[cfg(feature = "embassy-time")]
    liveness: Option<LivenessConfig>,
    #[cfg(feature = "embassy-time")]
    state_notify: Option<&'static WaitQueue>,
    #[cfg(feature = "embassy-time")]
    have_received: bool,
    #[cfg(feature = "embassy-time")]
    needs_cobs_reset: bool,
}

impl<N, R> RxWorker<N, R>
where
    N: NetStackHandle,
    R: Read,
{
    /// Create a new RX worker in target mode.
    ///
    /// In target mode, we will receive our net_id/node_id assignments from our
    /// controller.
    pub fn new_target(
        net: N,
        rx: R,
        ident: <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent,
    ) -> Self {
        Self {
            nsh: net,
            rx,
            net_id: None,
            ident,
            is_controller: false,
            #[cfg(feature = "embassy-time")]
            liveness: None,
            #[cfg(feature = "embassy-time")]
            state_notify: None,
            #[cfg(feature = "embassy-time")]
            have_received: false,
            #[cfg(feature = "embassy-time")]
            needs_cobs_reset: false,
        }
    }

    /// Create a new RX worker in controller mode.
    ///
    /// In controller mode, we will hardcode our net_id/node_id to `1.1`, allowing
    /// us to speak to a target.
    pub fn new_controller(
        net: N,
        rx: R,
        ident: <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent,
    ) -> Self {
        Self {
            nsh: net,
            rx,
            net_id: None,
            ident,
            is_controller: true,
            #[cfg(feature = "embassy-time")]
            liveness: None,
            #[cfg(feature = "embassy-time")]
            state_notify: None,
            #[cfg(feature = "embassy-time")]
            have_received: false,
            #[cfg(feature = "embassy-time")]
            needs_cobs_reset: false,
        }
    }

    /// Set liveness tracking configuration.
    ///
    /// When enabled, the RxWorker transitions the interface to `Inactive` if no
    /// frames are received within `config.timeout_ms`. The timer only starts
    /// after the first frame is received. Recovery is automatic — when frames
    /// resume, `process_frame` transitions back to `Active`.
    #[cfg(feature = "embassy-time")]
    pub fn with_liveness(mut self, config: LivenessConfig) -> Self {
        self.liveness = Some(config);
        self
    }

    /// Set a WaitQueue to be notified on interface state transitions.
    #[cfg(feature = "embassy-time")]
    pub fn with_state_notify(mut self, notify: &'static WaitQueue) -> Self {
        self.state_notify = Some(notify);
        self
    }

    pub async fn run(&mut self, frame: &mut [u8], scratch: &mut [u8]) -> Result<(), R::Error> {
        // Mark the interface as established
        _ = self.nsh.stack().manage_profile(|im| {
            if self.is_controller {
                self.net_id = Some(1);
                im.set_interface_state(
                    self.ident.clone(),
                    InterfaceState::Active {
                        net_id: 1,
                        node_id: CENTRAL_NODE_ID,
                    },
                )
            } else {
                self.net_id = None;
                im.set_interface_state(self.ident.clone(), InterfaceState::Inactive)
            }
        });
        #[cfg(feature = "embassy-time")]
        if let Some(notify) = self.state_notify {
            notify.wake_all();
        }
        let res = self.run_inner(frame, scratch).await;
        _ = self
            .nsh
            .stack()
            .manage_profile(|im| im.set_interface_state(self.ident.clone(), InterfaceState::Down));
        #[cfg(feature = "embassy-time")]
        if let Some(notify) = self.state_notify {
            notify.wake_all();
        }
        res
    }

    pub async fn run_inner(
        &mut self,
        frame: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<(), R::Error> {
        let mut acc = CobsAccumulator::new(frame);

        'outer: loop {
            let used = self.read_or_timeout(scratch).await?;

            // After liveness timeout, flush stale COBS state
            #[cfg(feature = "embassy-time")]
            if self.needs_cobs_reset {
                acc.reset();
                self.needs_cobs_reset = false;
            }
            let mut remain = &mut scratch[..used];

            loop {
                match acc.feed_raw(remain) {
                    FeedResult::Consumed => continue 'outer,
                    FeedResult::OverFull(items) => {
                        remain = items;
                    }
                    FeedResult::DecodeError(items) => {
                        remain = items;
                    }
                    FeedResult::Success { data, remaining }
                    | FeedResult::SuccessInput { data, remaining } => {
                        let prev_net_id = self.net_id;
                        process_frame(&mut self.net_id, data, &self.nsh, self.ident.clone());
                        #[cfg(feature = "embassy-time")]
                        {
                            self.have_received = true;
                        }
                        // Notify on state transition (net_id changed means Active)
                        #[cfg(feature = "embassy-time")]
                        if self.net_id != prev_net_id
                            && let Some(notify) = self.state_notify
                        {
                            notify.wake_all();
                        }
                        let _ = prev_net_id;
                        remain = remaining;
                    }
                }
            }
        }
    }

    /// Read from the transport with optional liveness timeout.
    ///
    /// Without `embassy-time` feature, this is a plain read.
    /// With `embassy-time` and liveness configured, transitions to `Inactive`
    /// on timeout (only after first frame received). Loops back waiting for
    /// frames or another timeout — the transport read error is the only exit.
    async fn read_or_timeout(&mut self, scratch: &mut [u8]) -> Result<usize, R::Error> {
        #[cfg(feature = "embassy-time")]
        {
            loop {
                let liveness_active = self.liveness.is_some() && self.have_received;
                if liveness_active {
                    let timeout_ms = self.liveness.as_ref().unwrap().timeout_ms;
                    let duration = embassy_time::Duration::from_millis(timeout_ms);
                    match embassy_time::with_timeout(duration, self.rx.read(scratch)).await {
                        Ok(result) => return result,
                        Err(_timeout) => {
                            // Liveness timeout — transition Active → Inactive
                            let changed = self.nsh.stack().manage_profile(|im| {
                                if matches!(
                                    im.interface_state(self.ident.clone()),
                                    Some(InterfaceState::Active { .. })
                                ) {
                                    _ = im.set_interface_state(
                                        self.ident.clone(),
                                        InterfaceState::Inactive,
                                    );
                                    true
                                } else {
                                    false
                                }
                            });
                            if changed && let Some(notify) = self.state_notify {
                                notify.wake_all();
                            }
                            // Reset so process_frame re-establishes Active on next frame
                            self.net_id = None;
                            self.have_received = false;
                            self.needs_cobs_reset = true;
                            continue;
                        }
                    }
                } else {
                    return self.rx.read(scratch).await;
                }
            }
        }
        #[cfg(not(feature = "embassy-time"))]
        {
            self.rx.read(scratch).await
        }
    }
}

impl<N, R> Drop for RxWorker<N, R>
where
    N: NetStackHandle,
    R: Read,
{
    fn drop(&mut self) {
        self.nsh.stack().manage_profile(|im| {
            _ = im.set_interface_state(self.ident.clone(), InterfaceState::Down);
        });
        #[cfg(feature = "embassy-time")]
        if let Some(notify) = self.state_notify {
            notify.wake_all();
        }
    }
}
