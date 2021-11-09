//! Types and code for mapping StreamIDs to streams on a circuit.

use crate::circuit::halfstream::HalfStream;
use crate::circuit::sendme;
use crate::{Error, Result};
/// Mapping from stream ID to streams.
// NOTE: This is a work in progress and I bet I'll refactor it a lot;
// it needs to stay opaque!
use tor_cell::relaycell::{msg::RelayMsg, StreamId};

use futures::channel::mpsc;
use std::collections::hash_map::Entry;
use std::collections::HashMap;

use rand::Rng;

use tracing::info;

/// The entry for a stream.
pub(super) enum StreamEnt {
    /// An open stream.
    Open {
        /// Sink to send relay cells tagged for this stream into.
        sink: mpsc::UnboundedSender<RelayMsg>,
        /// Stream for cells that should be sent down this stream.
        rx: mpsc::Receiver<RelayMsg>,
        /// Send window, for congestion control purposes.
        send_window: sendme::StreamSendWindow,
        /// Receive window, for congestion control purposes.
        recv_window: sendme::StreamRecvWindow,
        /// Number of cells dropped due to the stream disappearing before we can
        /// transform this into an `EndSent`.
        dropped: u16,
    },
    /// A stream for which we have received an END cell, but not yet
    /// had the stream object get dropped.
    EndReceived,
    /// A stream for which we have sent an END cell but not yet received
    /// an END cell.
    ///
    /// XXXX Can we ever throw this out? Do we really get END cells for these?
    EndSent(HalfStream),
}

/// Return value to indicate whether or not we send an END cell upon
/// terminating a given stream.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(super) enum ShouldSendEnd {
    /// An END cell should be sent.
    Send,
    /// An END cell should not be sent.
    DontSend,
}

/// A map from stream IDs to stream entries. Each circuit has one for each
/// hop.
pub(super) struct StreamMap {
    /// Map from StreamId to StreamEnt.  If there is no entry for a
    /// StreamId, that stream doesn't exist.
    m: HashMap<StreamId, StreamEnt>,
    /// The next StreamId that we should use for a newly allocated
    /// circuit.  (0 is not a valid streamID).
    next_stream_id: u16,
}

impl StreamMap {
    /// Make a new empty StreamMap.
    pub(super) fn new() -> Self {
        let mut rng = rand::thread_rng();
        let next_stream_id: u16 = loop {
            let v: u16 = rng.gen();
            if v != 0 {
                break v;
            }
        };
        StreamMap {
            m: HashMap::new(),
            next_stream_id,
        }
    }

    pub(super) fn inner(&mut self) -> &mut HashMap<StreamId, StreamEnt> {
        &mut self.m
    }

    /// Add an entry to this map; return the newly allocated StreamId.
    pub(super) fn add_ent(
        &mut self,
        sink: mpsc::UnboundedSender<RelayMsg>,
        rx: mpsc::Receiver<RelayMsg>,
        send_window: sendme::StreamSendWindow,
        recv_window: sendme::StreamRecvWindow,
    ) -> Result<StreamId> {
        let stream_ent = StreamEnt::Open {
            sink,
            rx,
            send_window,
            recv_window,
            dropped: 0,
        };
        // This "65536" seems too aggressive, but it's what tor does.
        //
        // Also, going around in a loop here is (sadly) needed in order
        // to look like Tor clients.
        for _ in 1..=65536 {
            let id: StreamId = self.next_stream_id.into();
            self.next_stream_id = self.next_stream_id.wrapping_add(1);
            if id.is_zero() {
                continue;
            }
            let ent = self.m.entry(id);
            if let Entry::Vacant(_) = ent {
                ent.or_insert(stream_ent);
                return Ok(id);
            }
        }

        Err(Error::IdRangeFull)
    }

    /// Return the entry for `id` in this map, if any.
    pub(super) fn get_mut(&mut self, id: StreamId) -> Option<&mut StreamEnt> {
        self.m.get_mut(&id)
    }

    /// Note that we received an END cell on the stream with `id`.
    ///
    /// Returns true if there was really a stream there.
    pub(super) fn end_received(&mut self, id: StreamId) -> Result<()> {
        // Check the hashmap for the right stream. Bail if not found.
        // Also keep the hashmap handle so that we can do more efficient inserts/removals
        let mut stream_entry = match self.m.entry(id) {
            Entry::Vacant(_) => {
                return Err(Error::CircProto(
                    "Received END cell on nonexistent stream".into(),
                ))
            }
            Entry::Occupied(o) => o,
        };

        // Progress the stream's state machine accordingly
        match stream_entry.get() {
            StreamEnt::EndReceived => Err(Error::CircProto(
                "Received two END cells on same stream".into(),
            )),
            StreamEnt::EndSent(_) => {
                info!("Actually got an end cell on a half-closed stream!");
                // We got an END, and we already sent an END. Great!
                // we can forget about this stream.
                stream_entry.remove_entry();
                Ok(())
            }
            StreamEnt::Open { .. } => {
                stream_entry.insert(StreamEnt::EndReceived);
                Ok(())
            }
        }
    }

    /// Handle a termination of the stream with `id` from this side of
    /// the circuit. Return true if the stream was open and an END
    /// ought to be sent.
    pub(super) fn terminate(&mut self, id: StreamId) -> Result<ShouldSendEnd> {
        // Progress the stream's state machine accordingly
        match self.m.remove(&id).ok_or_else(|| {
            Error::InternalError("Somehow we terminated a nonexistent connection‽".into())
        })? {
            StreamEnt::EndReceived => Ok(ShouldSendEnd::DontSend),
            StreamEnt::Open {
                send_window,
                mut recv_window,
                dropped,
                // notably absent: the channels for sink and stream, which will get dropped and
                // closed (meaning reads/writes from/to this stream will now fail)
                ..
            } => {
                recv_window.decrement_n(dropped)?;
                // TODO: would be nice to avoid new_ref.
                // XXXX: We should set connected_ok properly.
                let connected_ok = true;
                let halfstream = HalfStream::new(send_window, recv_window, connected_ok);
                self.m.insert(id, StreamEnt::EndSent(halfstream));
                Ok(ShouldSendEnd::Send)
            }
            StreamEnt::EndSent(_) => {
                panic!("Hang on! We're sending an END on a stream where we already sent an END‽");
            }
        }
    }

    // TODO: Eventually if we want relay support, we'll need to support
    // stream IDs chosen by somebody else. But for now, we don't need those.
}

#[cfg(test)]
mod test {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::circuit::sendme::{StreamRecvWindow, StreamSendWindow};

    #[test]
    fn streammap_basics() -> Result<()> {
        let mut map = StreamMap::new();
        let mut next_id = map.next_stream_id;
        let mut ids = Vec::new();

        // Try add_ent
        for _ in 0..128 {
            let (sink, _) = mpsc::unbounded();
            let (_, rx) = mpsc::channel(2);
            let id = map.add_ent(
                sink,
                rx,
                StreamSendWindow::new(500),
                StreamRecvWindow::new(500),
            )?;
            let expect_id: StreamId = next_id.into();
            assert_eq!(expect_id, id);
            next_id = next_id.wrapping_add(1);
            if next_id == 0 {
                next_id = 1;
            }
            ids.push(id);
        }

        // Test get_mut.
        let nonesuch_id = next_id.into();
        assert!(matches!(map.get_mut(ids[0]), Some(StreamEnt::Open { .. })));
        assert!(map.get_mut(nonesuch_id).is_none());

        // Test end_received
        assert!(map.end_received(nonesuch_id).is_err());
        assert!(map.end_received(ids[1]).is_ok());
        assert!(matches!(map.get_mut(ids[1]), Some(StreamEnt::EndReceived)));
        assert!(map.end_received(ids[1]).is_err());

        // Test terminate
        assert!(map.terminate(nonesuch_id).is_err());
        assert_eq!(map.terminate(ids[2]).unwrap(), ShouldSendEnd::Send);
        assert!(matches!(map.get_mut(ids[2]), Some(StreamEnt::EndSent(_))));
        assert_eq!(map.terminate(ids[1]).unwrap(), ShouldSendEnd::DontSend);
        assert!(matches!(map.get_mut(ids[1]), None));

        // Try receiving an end after a terminate.
        assert!(map.end_received(ids[2]).is_ok());
        assert!(matches!(map.get_mut(ids[2]), None));

        Ok(())
    }
}
