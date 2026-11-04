//! The unknown-call buffer.
//!
//! A `call_keys_delivered` for a `call_id` the client does not know yet (the gateway can beat the
//! join's HTTP answer) is buffered for `params.unknown_call_buffer_ms`, then applied once the
//! session exists, or dropped. Bounded: at most 64 messages or 1 MiB per unknown call and at
//! most 2 unknown calls; the oldest is dropped and counted.

use std::collections::VecDeque;

use sew_proto::e2ee::{CallId, CallKeysMessage};

use crate::config::{E2eeConfig, Params};
use crate::ids::Millis;

/// The `call_id` a dispatch names (`revoked` names none).
pub fn call_id_of(m: &CallKeysMessage) -> Option<&CallId> {
    match m {
        CallKeysMessage::Init(i) => Some(&i.call_id),
        CallKeysMessage::Proposals { call_id, .. }
        | CallKeysMessage::CommitRequest { call_id, .. }
        | CallKeysMessage::Commit { call_id, .. }
        | CallKeysMessage::Welcome { call_id, .. }
        | CallKeysMessage::Activate { call_id, .. }
        | CallKeysMessage::Reset { call_id, .. }
        | CallKeysMessage::Recorder { call_id, .. }
        | CallKeysMessage::Removed { call_id, .. }
        | CallKeysMessage::Resync { call_id, .. } => Some(call_id),
        CallKeysMessage::Revoked { .. } | CallKeysMessage::Unknown => None,
    }
}

#[derive(Debug)]
struct Pending {
    call_id: CallId,
    first_at: Millis,
    bytes: usize,
    messages: VecDeque<(Millis, usize, CallKeysMessage)>,
}

/// Buffers dispatches for calls not known yet.
#[derive(Debug)]
pub struct UnknownCallBuffer {
    window_ms: u64,
    max_messages: usize,
    max_bytes: usize,
    max_calls: usize,
    calls: VecDeque<Pending>,
    dropped: u64,
}

impl UnknownCallBuffer {
    /// A buffer with the server's window and the local bounds.
    pub fn new(params: &Params, cfg: &E2eeConfig) -> UnknownCallBuffer {
        UnknownCallBuffer {
            window_ms: params.unknown_call_buffer_ms,
            max_messages: cfg.unknown_call_max_messages,
            max_bytes: cfg.unknown_call_max_bytes,
            max_calls: cfg.unknown_call_max_calls,
            calls: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Buffers `msg` (`size` = its raw JSON length) for `call_id`.
    pub fn push(&mut self, now: Millis, call_id: CallId, msg: CallKeysMessage, size: usize) {
        self.expire(now);
        let idx = match self.calls.iter().position(|p| p.call_id == call_id) {
            Some(i) => i,
            None => {
                while self.calls.len() >= self.max_calls {
                    if let Some(old) = self.calls.pop_front() {
                        self.dropped += old.messages.len() as u64;
                    }
                }
                self.calls.push_back(Pending {
                    call_id,
                    first_at: now,
                    bytes: 0,
                    messages: VecDeque::new(),
                });
                self.calls.len() - 1
            }
        };
        let (max_messages, max_bytes) = (self.max_messages, self.max_bytes);
        let Some(p) = self.calls.get_mut(idx) else {
            return;
        };
        p.messages.push_back((now, size, msg));
        p.bytes += size;
        while p.messages.len() > max_messages || p.bytes > max_bytes {
            if let Some((_, s, _)) = p.messages.pop_front() {
                p.bytes -= s;
                self.dropped += 1;
            } else {
                break;
            }
        }
    }

    /// Drops everything older than the window.
    pub fn expire(&mut self, now: Millis) {
        let window = self.window_ms;
        let mut dropped = 0u64;
        for p in &mut self.calls {
            while p
                .messages
                .front()
                .is_some_and(|(t, _, _)| now.saturating_sub(*t) > window)
            {
                if let Some((_, s, _)) = p.messages.pop_front() {
                    p.bytes -= s;
                    dropped += 1;
                }
            }
        }
        self.dropped += dropped;
        self.calls.retain(|p| !p.messages.is_empty());
    }

    /// Takes the buffered messages of `call_id`, in order (the session now exists).
    pub fn take(&mut self, now: Millis, call_id: &CallId) -> Vec<CallKeysMessage> {
        self.expire(now);
        match self.calls.iter().position(|p| &p.call_id == call_id) {
            Some(i) => self
                .calls
                .remove(i)
                .map(|p| p.messages.into_iter().map(|(_, _, m)| m).collect())
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Messages dropped (window, bounds, evicted calls): `unknown_call_dropped`.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Messages and bytes held for `call_id`.
    pub fn held(&self, call_id: &CallId) -> (usize, usize) {
        self.calls
            .iter()
            .find(|p| &p.call_id == call_id)
            .map_or((0, 0), |p| (p.messages.len(), p.bytes))
    }

    /// Calls held.
    pub fn calls(&self) -> usize {
        self.calls.len()
    }

    /// When the oldest held message expires.
    pub fn next_expiry(&self) -> Option<Millis> {
        self.calls
            .iter()
            .filter_map(|p| p.messages.front().map(|(t, _, _)| t + self.window_ms + 1))
            .min()
            .or_else(|| self.calls.front().map(|p| p.first_at + self.window_ms + 1))
    }
}
