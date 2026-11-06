use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{CarabinerConnection, LinkStatus, LinkStatusError};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const STATUS_MAX_AGE: Duration = Duration::from_millis(1500);
const RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_STATUS_PER_POLL: u32 = 64;

/// Current status of an enabled Link observer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LinkObservation {
    /// The worker is waiting for its first observation.
    Connecting,
    /// The local bridge is unavailable or its observation has expired.
    Unavailable,
    /// The bridge reports a tempo but sees no other Link peers.
    NoPeers {
        /// Current tempo reported by the bridge.
        tempo_bpm: f64,
    },
    /// The bridge reports a current tempo and one or more Link peers.
    Following {
        /// Number of other peers reported by the bridge.
        peers: u32,
        /// Current session tempo.
        tempo_bpm: f64,
    },
}

impl LinkObservation {
    /// Return a target tempo only while following a peer session.
    pub fn target_tempo(self) -> Option<f64> {
        match self {
            Self::Following { tempo_bpm, .. } => Some(tempo_bpm),
            _ => None,
        }
    }
}

/// Owns the opt-in background observer and its latest Link status.
///
/// Starting creates a worker. Dropping requests stop without joining on the caller's thread.
pub struct LinkObserver {
    shared: Arc<Shared>,
    worker: JoinHandle<()>,
}

impl LinkObserver {
    /// Start observing the separately run bridge on the default loopback endpoint.
    pub fn start() -> io::Result<Self> {
        Self::spawn(CarabinerConnection::connect)
    }

    fn spawn(
        connect: impl FnMut() -> Result<CarabinerConnection, LinkStatusError> + Send + 'static,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            snapshot: Mutex::new(Snapshot::Connecting),
        });
        let thread_state = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name(String::from("yield-link"))
            .spawn(move || observe(&thread_state, connect))?;
        Ok(Self { shared, worker })
    }

    /// Read the latest non-stale observation without waiting on the worker.
    ///
    /// `None` means the snapshot could not be read immediately.
    pub fn observation(&self) -> Option<LinkObservation> {
        let snapshot = self.shared.snapshot.try_lock().ok()?;
        Some(snapshot.observation(Instant::now()))
    }
}

impl Drop for LinkObserver {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.worker.thread().unpark();
    }
}

struct Shared {
    stopped: AtomicBool,
    snapshot: Mutex<Snapshot>,
}

impl Shared {
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn publish(&self, next: Snapshot) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            *snapshot = next;
        }
    }
}

enum Snapshot {
    Connecting,
    Unavailable,
    Seen { status: LinkStatus, at: Instant },
}

impl Snapshot {
    fn observation(&self, now: Instant) -> LinkObservation {
        match *self {
            Self::Connecting => LinkObservation::Connecting,
            Self::Seen { status, at } if now.saturating_duration_since(at) <= STATUS_MAX_AGE => {
                if status.peers == 0 {
                    LinkObservation::NoPeers {
                        tempo_bpm: status.tempo_bpm,
                    }
                } else {
                    LinkObservation::Following {
                        peers: status.peers,
                        tempo_bpm: status.tempo_bpm,
                    }
                }
            }
            Self::Seen { .. } | Self::Unavailable => LinkObservation::Unavailable,
        }
    }
}

fn observe(
    shared: &Shared,
    mut connect: impl FnMut() -> Result<CarabinerConnection, LinkStatusError>,
) {
    while !shared.stopped() {
        if let Ok(mut connection) = connect() {
            observe_connection(shared, &mut connection);
        }
        if shared.stopped() {
            break;
        }
        shared.publish(Snapshot::Unavailable);
        let retry_at = Instant::now() + RETRY_DELAY;
        while !shared.stopped() {
            let remaining = retry_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            thread::park_timeout(remaining);
        }
    }
}

fn observe_connection(shared: &Shared, connection: &mut CarabinerConnection) {
    let mut poll_at = Instant::now();
    let mut status_count = 0;
    while !shared.stopped() {
        let now = Instant::now();
        if now >= poll_at {
            if connection.request_status().is_err() {
                break;
            }
            poll_at = now + POLL_INTERVAL;
            status_count = 0;
        }
        match connection.read_status() {
            Ok(Some(status)) => {
                status_count += 1;
                if shared.stopped() || status_count > MAX_STATUS_PER_POLL {
                    break;
                }
                shared.publish(Snapshot::Seen {
                    status,
                    at: Instant::now(),
                });
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener, TcpStream};

    const FIXTURE_TIMEOUT: Duration = Duration::from_millis(300);

    #[test]
    fn observer_publishes_peer_transitions_and_revokes_tempo_on_disconnect() {
        let (observer, mut bridge) = fixture_bridge();
        bridge.write_all(b"status { :peers 0 :bpm 120 }\n").unwrap();
        wait_for(&observer, LinkObservation::NoPeers { tempo_bpm: 120.0 });
        assert_eq!(observer.observation().unwrap().target_tempo(), None);

        bridge
            .write_all(b"status { :peers 2 :bpm 132.5 }\n")
            .unwrap();
        let following = LinkObservation::Following {
            peers: 2,
            tempo_bpm: 132.5,
        };
        wait_for(&observer, following);
        assert_eq!(observer.observation().unwrap().target_tempo(), Some(132.5));

        drop(bridge);
        wait_for(&observer, LinkObservation::Unavailable);
        assert_eq!(observer.observation().unwrap().target_tempo(), None);
    }

    #[test]
    fn observer_stops_access_after_drop() {
        let (observer, mut bridge) = fixture_bridge();
        drop(observer);
        let mut byte = [0];
        assert_eq!(bridge.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn excessive_status_burst_revokes_the_observation() {
        let (observer, mut bridge) = fixture_bridge();
        let status = b"status { :peers 1 :bpm 120 }\n";
        bridge
            .write_all(&status.repeat((MAX_STATUS_PER_POLL + 1) as usize))
            .unwrap();
        wait_for(&observer, LinkObservation::Unavailable);
    }

    #[test]
    fn stale_observation_cannot_supply_a_target_tempo() {
        let at = Instant::now();
        let snapshot = Snapshot::Seen {
            status: LinkStatus {
                peers: 1,
                tempo_bpm: 120.0,
            },
            at,
        };
        assert_eq!(
            snapshot.observation(at + STATUS_MAX_AGE).target_tempo(),
            Some(120.0)
        );
        let expired = snapshot.observation(at + STATUS_MAX_AGE + Duration::from_nanos(1));
        assert_eq!(expired, LinkObservation::Unavailable);
        assert_eq!(expired.target_tempo(), None);
    }

    fn fixture_bridge() -> (LinkObserver, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut connection = Some(
            CarabinerConnection::connect_to_loopback(listener.local_addr().unwrap().port())
                .unwrap(),
        );
        let (mut bridge, _) = listener.accept().unwrap();
        bridge.set_read_timeout(Some(FIXTURE_TIMEOUT)).unwrap();
        bridge.set_write_timeout(Some(FIXTURE_TIMEOUT)).unwrap();
        let observer =
            LinkObserver::spawn(move || connection.take().ok_or(LinkStatusError::Disconnected))
                .unwrap();
        let mut request = [0; 7];
        bridge.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"status\n");
        (observer, bridge)
    }

    fn wait_for(observer: &LinkObserver, expected: LinkObservation) {
        let deadline = Instant::now() + FIXTURE_TIMEOUT;
        loop {
            if observer.observation() == Some(expected) {
                return;
            }
            assert!(Instant::now() < deadline, "missing Link state {expected:?}");
            thread::sleep(Duration::from_millis(1));
        }
    }
}
