#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

use polly::event_manager::{EventManager, Pollable, Subscriber};
use utils::epoll::{EpollEvent, EventSet};
use utils::eventfd::EventFd;

use super::device::{Rng, REQ_INDEX};
use crate::virtio::device::VirtioDevice;

/// Returns the platform-agnostic pollable identifier for an EventFd.
#[cfg(unix)]
fn eventfd_pollable(efd: &EventFd) -> Pollable {
    efd.as_raw_fd()
}

#[cfg(windows)]
fn eventfd_pollable(efd: &EventFd) -> Pollable {
    efd.as_raw_handle() as Pollable
}

impl Rng {
    pub(crate) fn handle_req_event(&mut self, event: &EpollEvent) {
        debug!("rng: request queue event");

        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("rng: request queue unexpected event {event_set:?}");
            return;
        }

        if let Err(e) = self.queue_event(REQ_INDEX).read() {
            error!("Failed to read request queue event: {e:?}");
        } else if self.process_req() {
            self.device_state.signal_used_queue();
        }
    }

    fn handle_activate_event(&self, event_manager: &mut EventManager) {
        debug!("rng: activate event");
        if let Err(e) = self.activate_evt.read() {
            error!("Failed to consume rng activate event: {e:?}");
        }

        // The subscriber must exist as we previously registered activate_evt via
        // `interest_list()`.
        let self_subscriber = event_manager
            .subscriber(eventfd_pollable(&self.activate_evt))
            .unwrap();

        event_manager
            .register(
                eventfd_pollable(self.queue_event(REQ_INDEX)),
                EpollEvent::new(EventSet::IN, eventfd_pollable(self.queue_event(REQ_INDEX)) as u64),
                self_subscriber.clone(),
            )
            .unwrap_or_else(|e| {
                error!("Failed to register rng frq with event manager: {e:?}");
            });

        event_manager
            .unregister(eventfd_pollable(&self.activate_evt))
            .unwrap_or_else(|e| {
                error!("Failed to unregister rng activate evt: {e:?}");
            })
    }
}

impl Subscriber for Rng {
    fn process(&mut self, event: &EpollEvent, event_manager: &mut EventManager) {
        let source = event.fd();
        let req = eventfd_pollable(self.queue_event(REQ_INDEX));
        let activate_evt = eventfd_pollable(&self.activate_evt);

        if self.is_activated() {
            match source {
                _ if source == req => self.handle_req_event(event),
                _ if source == activate_evt => {
                    self.handle_activate_event(event_manager);
                }
                _ => warn!("Unexpected rng event received: {source:?}"),
            }
        } else {
            warn!("rng: The device is not yet activated. Spurious event received: {source:?}");
        }
    }

    fn interest_list(&self) -> Vec<EpollEvent> {
        vec![EpollEvent::new(
            EventSet::IN,
            eventfd_pollable(&self.activate_evt) as u64,
        )]
    }
}
