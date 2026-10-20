//! The Pier service's signals, installed before any start-up work.
//!
//! systemd and logrotate signal the service whenever they like: `SIGHUP`
//! from logrotate's postrotate and `systemctl reload`, `SIGTERM` from
//! `systemctl stop`. Until a handler exists a signal has its default action
//! and ends the process, and systemd counts a `SIGHUP` death as a clean
//! exit that no restart policy restarts. Start-up can take minutes (an
//! automatic Wayland re-qualification), so [`ServiceSignals::install`] runs
//! as soon as the runtime exists. From then on, during start-up, `SIGHUP`
//! reopens the log and reloads logging; the TLS and QoS reload it also
//! means while serving needs nothing, because both are read from their
//! files when the listener starts. `SIGTERM`/`SIGINT` request an orderly
//! stop that long start-up work observes, and that the listener honours.

use std::future::Future;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::logging::{target, LogController};

/// The signal that asked the service to stop, once one has.
pub type StopRequest = Option<&'static str>;

/// The service's signal handlers and the stop they request.
pub struct ServiceSignals {
    #[cfg(unix)]
    hangup: Option<tokio::signal::unix::Signal>,
    stop: watch::Receiver<StopRequest>,
}

impl ServiceSignals {
    /// Installs the handlers. Runs inside the Tokio runtime.
    #[must_use]
    pub fn install() -> Self {
        let (requested, stop) = watch::channel(None);
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let hangup = match signal(SignalKind::hangup()) {
                Ok(signal) => Some(signal),
                Err(error) => {
                    warn!(target: target::NET, %error, "SIGHUP reload is unavailable");
                    None
                }
            };
            match (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
            ) {
                (Ok(mut terminate), Ok(mut interrupt)) => {
                    tokio::spawn(async move {
                        loop {
                            let name = tokio::select! {
                                _ = terminate.recv() => "SIGTERM",
                                _ = interrupt.recv() => "SIGINT",
                            };
                            // The first request stands; later ones change nothing.
                            requested.send_if_modified(|current| {
                                current.is_none() && {
                                    *current = Some(name);
                                    true
                                }
                            });
                        }
                    });
                }
                (Err(error), _) | (_, Err(error)) => {
                    warn!(
                        target: target::NET,
                        %error,
                        "SIGTERM/SIGINT handlers are unavailable; stopping on Ctrl-C only"
                    );
                    spawn_ctrl_c(requested);
                }
            }
            Self { hangup, stop }
        }
        #[cfg(not(unix))]
        {
            spawn_ctrl_c(requested);
            Self { stop }
        }
    }

    /// The signal that requested a stop, if one has.
    #[must_use]
    pub fn stop_requested(&self) -> StopRequest {
        *self.stop.borrow()
    }

    /// A receiver that start-up work watches to abandon itself in order.
    #[must_use]
    pub fn stop_receiver(&self) -> watch::Receiver<StopRequest> {
        self.stop.clone()
    }

    /// Resolves once a stop is requested, with the signal's name.
    pub async fn stopped(&self) -> &'static str {
        stopped(self.stop.clone()).await
    }

    /// Hands the `SIGHUP` stream to the listener's reload loop.
    #[cfg(unix)]
    pub fn take_hangup(&mut self) -> Option<tokio::signal::unix::Signal> {
        self.hangup.take()
    }

    /// Awaits start-up `work`; a `SIGHUP` meanwhile reopens the log instead
    /// of ending the process. A stop request is left to `work` (which
    /// watches [`Self::stop_receiver`]) and to the caller.
    pub async fn during_startup<F: Future>(
        &mut self,
        log_controller: &LogController,
        work: F,
    ) -> F::Output {
        tokio::pin!(work);
        loop {
            #[cfg(unix)]
            let hangup = async {
                match self.hangup.as_mut() {
                    Some(signal) => signal.recv().await,
                    None => std::future::pending().await,
                }
            };
            #[cfg(not(unix))]
            let hangup = std::future::pending::<Option<()>>();
            tokio::select! {
                output = &mut work => return output,
                Some(()) = hangup => reload_logging_during_startup(log_controller),
            }
        }
    }
}

/// Resolves once `stop` holds a request (never when no request can come).
pub async fn stopped(mut stop: watch::Receiver<StopRequest>) -> &'static str {
    match stop.wait_for(Option::is_some).await {
        Ok(requested) => requested.unwrap_or("stop"),
        Err(_) => std::future::pending().await,
    }
}

fn spawn_ctrl_c(requested: watch::Sender<StopRequest>) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = requested.send(Some("SIGINT"));
        }
        // Holding the sender keeps `stopped` from resolving on its own.
        std::future::pending::<()>().await;
    });
}

fn reload_logging_during_startup(log_controller: &LogController) {
    match log_controller.handle_sighup().error_message() {
        None => info!(
            target: target::NET,
            "SIGHUP during start-up: log reopened and logging reloaded; TLS and QoS settings \
             are read when the listener starts"
        ),
        Some(error) => warn!(
            target: target::NET,
            %error,
            "SIGHUP during start-up: logging maintenance partially failed"
        ),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn raise(signal: nix::sys::signal::Signal) {
        nix::sys::signal::raise(signal).expect("raise");
    }

    /// One test, so no other test in this process races the signals: a
    /// `SIGHUP` during start-up work does not end the process and does not
    /// interrupt the work; a `SIGTERM` is a stop request the work sees.
    #[test]
    fn startup_survives_sighup_and_sees_sigterm_as_a_stop_request() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut signals = ServiceSignals::install();
            assert_eq!(signals.stop_requested(), None);
            let options = crate::logging::LoggingOptions::from_args(&[]).expect("options");
            let log_controller = crate::logging::init_for_test(&options).expect("log controller");
            let stop = signals.stop_receiver();
            let work = async {
                raise(nix::sys::signal::Signal::SIGHUP);
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                raise(nix::sys::signal::Signal::SIGHUP);
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                assert_eq!(*stop.borrow(), None, "SIGHUP is not a stop");
                raise(nix::sys::signal::Signal::SIGTERM);
                tokio::time::timeout(std::time::Duration::from_secs(5), stopped(stop.clone()))
                    .await
                    .expect("SIGTERM requests a stop")
            };
            let name = signals.during_startup(&log_controller, work).await;
            assert_eq!(name, "SIGTERM");
            assert_eq!(signals.stop_requested(), Some("SIGTERM"));
            // A later SIGINT does not rewrite the request.
            raise(nix::sys::signal::Signal::SIGINT);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert_eq!(signals.stopped().await, "SIGTERM");
            assert!(signals.take_hangup().is_some());
        });
    }
}
