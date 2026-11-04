// Copyright 2026 Alakazam Labs
// SPDX-License-Identifier: Apache-2.0

//! Which LiveKit signal-protocol features a client can handle.
//!
//! A client states its protocol number in `ClientInfo.protocol` of its `JoinRequest`. Sew
//! admits it through [`ProtocolVersion::negotiate`], which refuses anything older than
//! [`MIN_CLIENT_PROTOCOL`] and caps newer clients at [`CURRENT_PROTOCOL`]. The engine then asks
//! the resulting [`ProtocolVersion`] about single features (for example
//! [`ProtocolVersion::supports_packet_trailer`]) instead of comparing numbers itself.
//!
//! Every feature turns on at one protocol number and stays on for every later one. The
//! features and where they start are listed once, in the table behind [`GATES`]; the methods
//! on [`ProtocolVersion`] are generated from that same table, so the two cannot disagree.
//! The methods look only at the number: they do not clamp it and they know nothing of the
//! floor, so they answer for any value, including one built by hand.

use std::fmt;

/// The signal protocol Sew's media engine speaks (that of the vendored protos at
/// [`crate::PROTOCOL_COMMIT`]). Newer clients are served at this level.
pub const CURRENT_PROTOCOL: i32 = 17;

/// The oldest client protocol Sew admits. This is Sew's own policy: the oldest client SDK
/// Sew supports (Android 2.29.0) declares 13.
pub const MIN_CLIENT_PROTOCOL: i32 = 13;

/// A client's signal protocol number, as declared or as negotiated.
///
/// Any `i32` can be wrapped; the feature methods work on all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion(pub i32);

/// Why [`ProtocolVersion::negotiate`] turned a client away: its protocol is older than
/// [`MIN_CLIENT_PROTOCOL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BelowFloor {
    /// The protocol number the client sent.
    pub declared: i32,
}

impl fmt::Display for BelowFloor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "client protocol {} is below the minimum {MIN_CLIENT_PROTOCOL}",
            self.declared
        )
    }
}

impl std::error::Error for BelowFloor {}

/// One row of the feature table: a [`ProtocolVersion`] method, the last protocol number
/// without the feature, and the method itself.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    /// The method's name on [`ProtocolVersion`].
    pub name: &'static str,
    /// The highest protocol number that lacks the feature; it is on from `threshold + 1`.
    pub threshold: i32,
    /// The method, callable as `(gate.check)(version)`.
    pub check: fn(ProtocolVersion) -> bool,
}

impl Gate {
    /// The first protocol number with the feature.
    #[must_use]
    pub const fn first_on(&self) -> i32 {
        self.threshold + 1
    }
}

impl ProtocolVersion {
    /// Admits a client by its declared protocol: `Err` when it is older than
    /// [`MIN_CLIENT_PROTOCOL`], otherwise the level the session runs at, which is the
    /// client's own number capped at [`CURRENT_PROTOCOL`].
    pub const fn negotiate(client: i32) -> Result<ProtocolVersion, BelowFloor> {
        if client < MIN_CLIENT_PROTOCOL {
            return Err(BelowFloor { declared: client });
        }
        let level = if client > CURRENT_PROTOCOL {
            CURRENT_PROTOCOL
        } else {
            client
        };
        Ok(ProtocolVersion(level))
    }

    /// True when this protocol is newer than `last_without`.
    const fn newer_than(self, last_without: i32) -> bool {
        self.0 > last_without
    }
}

/// Builds the feature methods and [`GATES`] from one list of
/// `doc; method_name, last protocol without the feature;` rows.
macro_rules! feature_table {
    ($( $doc:literal; $name:ident, $last_without:literal; )+) => {
        impl ProtocolVersion {
            $(
                #[doc = $doc]
                #[must_use]
                pub const fn $name(self) -> bool {
                    self.newer_than($last_without)
                }
            )+
        }

        /// Every feature method on [`ProtocolVersion`] with its threshold, grouped by area.
        pub const GATES: &[Gate] = &[
            $(
                Gate {
                    name: stringify!($name),
                    threshold: $last_without,
                    check: ProtocolVersion::$name,
                },
            )+
        ];
    };
}

feature_table! {
    // Encoding of the signal channel itself.
    "Signal messages are protobuf.";
        supports_protobuf, 0;
    "Stream ids may be sent packed.";
        supports_packed_stream_id, 0;
    "The server may answer a request with a non-error (OK) `RequestResponse`.";
        supports_non_error_signal_response, 14;

    // Peer connections and SDP.
    "The subscriber peer connection is the primary one.";
        subscriber_as_primary, 2;
    "The client reuses transceivers, which keeps SDPs small.";
        supports_transceiver_reuse, 3;
    "The client can use an ICE-lite server.";
        supports_ice_lite, 5;
    "The server may put media streams into its first offer.";
        support_fast_start, 7;
    "Stream ids are kept in step between the two sides.";
        supports_sync_stream_id, 9;

    // Data and media framing.
    "The client handles data packets.";
        handles_data_packets, 1;
    "Media may carry packet trailers.";
        supports_packet_trailer, 16;

    // Participant and room updates.
    "Active speakers arrive as changes rather than as the full list.";
        supports_speaker_changed, 2;
    "Tracks can be unpublished.";
        supports_unpublish, 6;
    "Participant updates may report participants as disconnected.";
        supports_disconnected_update, 8;
    "The room id may arrive later than the join.";
        supports_async_room_id, 11;

    // Connection quality.
    "The client takes connection-quality updates (frequent ones are skipped for older clients).";
        supports_connection_quality, 4;
    "The client knows the LOST connection-quality value (older clients get POOR instead).";
        supports_connection_quality_lost, 10;

    // Sessions, reconnects and leaving.
    "A session can migrate.";
        supports_session_migrate, 5;
    "A reconnect is matched by the participant's identity.";
        supports_identity_based_reconnection, 11;
    "A `LeaveRequest` may carry a list of regions.";
        supports_regions_in_leave_request, 12;
    "A participant can be moved to another room.";
        supports_moving, 15;
}
