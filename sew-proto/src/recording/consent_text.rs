//! The consent notice in English words, for native hosts (a lock screen, a notification) that
//! show [`ConsentPrompt`] without the web kit.
//!
//! The sentences are the web kit's consent modal's, word for word (sew-web
//! `packages/calls/src/strings.ts` `R` and `react.tsx` `ConsentDialog`, at c4f6512): the same
//! title, the same lines in the same order, the same answers. The prompt's slots fill them;
//! nothing here is sent to Sew, and the answer still echoes [`ConsentPrompt::notice_version`].

use super::{ConsentPrompt, DeclineMode};
use crate::id::Id;

/// Where the person meets the notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsentAsk {
    /// Before joining: the join is held until the answer (the kit's `join` mode; the plugin's
    /// `consent_required` state).
    Join,
    /// In the call: the recording started (or the notice changed) while the person is
    /// connected and `pending` (the kit's `room` mode).
    InCall,
}

/// One answer the notice offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsentChoice {
    /// The button's label.
    pub label: &'static str,
    /// The answer to send: `Some(true)` agrees, `Some(false)` declines; `None` sends nothing
    /// (cancel the join: leave the held call).
    pub granted: Option<bool>,
}

// The kit's strings (strings.ts `R`), verbatim.
const TITLE_ROOM: &str = "This call is being recorded";
const AUTO_START: &str = "A recording starts when someone joins this channel.";
const NOT_E2EE: &str = "Transcripts are not end-to-end encrypted.";
const ADMIN_DOWNLOAD: &str = "Authorised admins can download the audio.";
const NOBODY_YET: &str = "Nobody is recorded yet.";
const DECLINE_JOINS: &str = "If you don't allow it, you join without being recorded.";
const DECLINE_NOT_JOIN: &str = "In this channel you can't join without being recorded.";
const DECLINE_STAYS: &str = "If you don't allow it, you stay in the call without being recorded.";
const DECLINE_LEAVES: &str = "In this channel, not allowing it means leaving the call.";
const ALLOW_JOIN: &str = "Join and allow recording";
const JOIN_UNRECORDED: &str = "Join without being recorded";
const DONT_JOIN: &str = "Don't join";
const CANCEL: &str = "Cancel";
const ALLOW: &str = "Allow";
const DONT_RECORD_ME: &str = "Don't record me";
const LEAVE: &str = "Leave";

/// Who can read a transcript (strings.ts `readersText`).
fn readers(ids: &[String]) -> String {
    let xs: Vec<&str> = ids
        .iter()
        .map(|x| match x.as_str() {
            "channel_members" => "the channel's members",
            "search" => "search",
            "agents" => "agents",
            other => other,
        })
        .collect();
    match xs.as_slice() {
        [] => "the channel's members".to_owned(),
        [one] => (*one).to_owned(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

impl ConsentPrompt {
    /// The notice's title, as the kit's modal writes it: "Lounge is being recorded" (a
    /// recording someone started), "Calls in Lounge are recorded" (an auto-record channel's
    /// first join), "This call is being recorded" (in the call).
    ///
    /// A DM call has no room name ([`super::PromptRoom::name`]); where the kit puts the host's
    /// name for the conversation, this says "This call is being recorded".
    pub fn title_en(&self, ask: ConsentAsk) -> String {
        match (ask, self.room.name.as_deref()) {
            (ConsentAsk::InCall, _) | (ConsentAsk::Join, None) => TITLE_ROOM.to_owned(),
            (ConsentAsk::Join, Some(room)) if self.started_by.is_some() => {
                format!("{room} is being recorded")
            }
            (ConsentAsk::Join, Some(room)) => format!("Calls in {room} are recorded"),
        }
    }

    /// The line under the title: "Sew Notes, recorded by Bobbin".
    pub fn feature_en(&self) -> String {
        format!("{}, recorded by {}", self.feature_label, self.recorder_name)
    }

    /// The notice's sentences, in the kit's order: who started it, what the recorder does,
    /// the transcript and who reads it, encryption, how long audio is kept, admin download,
    /// who is recorded now, and what declining does. `member_name` names a member (the
    /// starter when Sew sent no display name, and each of [`ConsentPrompt::recorded_now`]); the
    /// host's names, as the kit's `memberName`.
    pub fn lines_en(&self, ask: ConsentAsk, member_name: &dyn Fn(Id) -> String) -> Vec<String> {
        let mut lines = Vec::new();
        match &self.started_by {
            Some(m) => {
                let name = if m.display_name.is_empty() {
                    member_name(m.member_id)
                } else {
                    m.display_name.clone()
                };
                lines.push(format!("{name} started recording this call."));
            }
            None => lines.push(AUTO_START.to_owned()),
        }
        lines.push(format!(
            "{} joins the call as a participant and records the voice of each person who agrees. \
             Nobody is recorded without saying yes.",
            self.recorder_name
        ));
        let who = readers(&self.transcript.readable_by);
        lines.push(if self.transcript.posted_to_channel {
            format!(
                "Each person's words are transcribed and posted into this channel as ordinary \
                 messages that {who} can read."
            )
        } else {
            format!("Each person's words are transcribed; {who} can read the transcript.")
        });
        if !self.transcript.e2ee {
            lines.push(NOT_E2EE.to_owned());
        }
        lines.push(match self.audio_keep_days {
            0 => "Audio is deleted once the transcript is final.".to_owned(),
            1 => "Audio is kept for 1 day.".to_owned(),
            n => format!("Audio is kept for {n} days."),
        });
        if self.admin_download {
            lines.push(ADMIN_DOWNLOAD.to_owned());
        }
        lines.push(if self.recorded_now.is_empty() {
            NOBODY_YET.to_owned()
        } else {
            let names: Vec<String> = self.recorded_now.iter().map(|m| member_name(*m)).collect();
            format!("Recorded now: {}.", names.join(", "))
        });
        let not_join = self.decline_mode == DeclineMode::NotJoin;
        lines.push(
            match (ask, not_join) {
                (ConsentAsk::Join, false) => DECLINE_JOINS,
                (ConsentAsk::Join, true) => DECLINE_NOT_JOIN,
                (ConsentAsk::InCall, false) => DECLINE_STAYS,
                (ConsentAsk::InCall, true) => DECLINE_LEAVES,
            }
            .to_owned(),
        );
        lines
    }

    /// The notice's body as one text: [`ConsentPrompt::feature_en`], then
    /// [`ConsentPrompt::lines_en`], one per line (`\n`).
    pub fn text_en(&self, ask: ConsentAsk, member_name: &dyn Fn(Id) -> String) -> String {
        let mut out = self.feature_en();
        for l in self.lines_en(ask, member_name) {
            out.push('\n');
            out.push_str(&l);
        }
        out
    }

    /// The answers the kit offers, in its order; none is a default. Before joining: "Join and
    /// allow recording", then "Join without being recorded" and "Cancel" (or only "Don't join"
    /// where declining means not joining). In the call: "Allow", then "Don't record me" (or
    /// "Leave" where declining means leaving).
    pub fn choices_en(&self, ask: ConsentAsk) -> Vec<ConsentChoice> {
        let c = |label, granted| ConsentChoice { label, granted };
        let not_join = self.decline_mode == DeclineMode::NotJoin;
        match (ask, not_join) {
            (ConsentAsk::Join, false) => vec![
                c(ALLOW_JOIN, Some(true)),
                c(JOIN_UNRECORDED, Some(false)),
                c(CANCEL, None),
            ],
            (ConsentAsk::Join, true) => vec![c(ALLOW_JOIN, Some(true)), c(DONT_JOIN, None)],
            (ConsentAsk::InCall, false) => {
                vec![c(ALLOW, Some(true)), c(DONT_RECORD_ME, Some(false))]
            }
            (ConsentAsk::InCall, true) => vec![c(ALLOW, Some(true)), c(LEAVE, Some(false))],
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{PromptMember, PromptRoom, PromptRoomKind, PromptTranscript, RecordingRef};
    use super::*;

    const BOB: u64 = 200;

    /// The kit's consent-modal test prompt (sew-web packages/calls/test/unit/consent-modal.test.tsx
    /// `P()`, c4f6512).
    fn kit_prompt() -> ConsentPrompt {
        ConsentPrompt {
            recording_id: RecordingRef::Id(Id::new(900).unwrap()),
            notice_version: 3,
            recorder_name: "Bobbin".into(),
            feature_label: "Sew Notes".into(),
            started_by: Some(PromptMember {
                member_id: Id::new(BOB).unwrap(),
                display_name: "Bob".into(),
            }),
            room: PromptRoom {
                channel_id: Id::new(48_831_765_147_549_698).unwrap(),
                kind: PromptRoomKind::Voice,
                name: Some("Lounge".into()),
            },
            audio_keep_days: 14,
            transcript: PromptTranscript {
                posted_to_channel: true,
                readable_by: vec!["channel_members".into(), "search".into(), "agents".into()],
                e2ee: false,
            },
            admin_download: true,
            decline_mode: DeclineMode::JoinUnrecorded,
            recorded_now: vec![Id::new(BOB).unwrap()],
        }
    }

    /// The kit test's `memberName`: Bob, else the id.
    fn names(id: Id) -> String {
        if id.get() == BOB {
            "Bob".into()
        } else {
            id.to_string()
        }
    }

    /// The golden: the kit modal's words at c4f6512, for its test prompt.
    #[test]
    fn join_notice_is_the_kits_modal() {
        let p = kit_prompt();
        assert_eq!(p.title_en(ConsentAsk::Join), "Lounge is being recorded");
        assert_eq!(p.feature_en(), "Sew Notes, recorded by Bobbin");
        let want = "Sew Notes, recorded by Bobbin\n\
            Bob started recording this call.\n\
            Bobbin joins the call as a participant and records the voice of each person who \
            agrees. Nobody is recorded without saying yes.\n\
            Each person's words are transcribed and posted into this channel as ordinary \
            messages that the channel's members, search and agents can read.\n\
            Transcripts are not end-to-end encrypted.\n\
            Audio is kept for 14 days.\n\
            Authorised admins can download the audio.\n\
            Recorded now: Bob.\n\
            If you don't allow it, you join without being recorded.";
        assert_eq!(p.text_en(ConsentAsk::Join, &names), want);
        let labels: Vec<_> = p
            .choices_en(ConsentAsk::Join)
            .iter()
            .map(|c| (c.label, c.granted))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("Join and allow recording", Some(true)),
                ("Join without being recorded", Some(false)),
                ("Cancel", None),
            ]
        );
    }

    #[test]
    fn declining_means_not_joining_or_leaving() {
        let mut p = kit_prompt();
        p.decline_mode = DeclineMode::NotJoin;
        let join = p.lines_en(ConsentAsk::Join, &names);
        assert_eq!(
            join.last().unwrap(),
            "In this channel you can't join without being recorded."
        );
        assert_eq!(
            p.choices_en(ConsentAsk::Join),
            vec![
                ConsentChoice {
                    label: "Join and allow recording",
                    granted: Some(true)
                },
                ConsentChoice {
                    label: "Don't join",
                    granted: None
                },
            ]
        );
        let in_call = p.lines_en(ConsentAsk::InCall, &names);
        assert_eq!(
            in_call.last().unwrap(),
            "In this channel, not allowing it means leaving the call."
        );
        assert_eq!(
            p.choices_en(ConsentAsk::InCall),
            vec![
                ConsentChoice {
                    label: "Allow",
                    granted: Some(true)
                },
                ConsentChoice {
                    label: "Leave",
                    granted: Some(false)
                },
            ]
        );
    }

    #[test]
    fn in_call_notice() {
        let p = kit_prompt();
        assert_eq!(
            p.title_en(ConsentAsk::InCall),
            "This call is being recorded"
        );
        assert_eq!(
            p.lines_en(ConsentAsk::InCall, &names).last().unwrap(),
            "If you don't allow it, you stay in the call without being recorded."
        );
        assert_eq!(
            p.choices_en(ConsentAsk::InCall),
            vec![
                ConsentChoice {
                    label: "Allow",
                    granted: Some(true)
                },
                ConsentChoice {
                    label: "Don't record me",
                    granted: Some(false)
                },
            ]
        );
    }

    /// An auto-record channel's first join, nobody recorded, other slot values: each sentence
    /// as the kit writes it.
    #[test]
    fn auto_channel_and_the_other_slot_values() {
        let mut p = kit_prompt();
        p.started_by = None;
        p.recorded_now = vec![];
        p.audio_keep_days = 0;
        p.transcript = PromptTranscript {
            posted_to_channel: false,
            readable_by: vec!["channel_members".into()],
            e2ee: true,
        };
        p.admin_download = false;
        assert_eq!(p.title_en(ConsentAsk::Join), "Calls in Lounge are recorded");
        assert_eq!(
            p.lines_en(ConsentAsk::Join, &names),
            vec![
                "A recording starts when someone joins this channel.",
                "Bobbin joins the call as a participant and records the voice of each person \
                 who agrees. Nobody is recorded without saying yes.",
                "Each person's words are transcribed; the channel's members can read the \
                 transcript.",
                "Audio is deleted once the transcript is final.",
                "Nobody is recorded yet.",
                "If you don't allow it, you join without being recorded.",
            ]
        );
        p.audio_keep_days = 1;
        p.transcript.readable_by = vec!["search".into(), "agents".into()];
        p.recorded_now = vec![Id::new(BOB).unwrap(), Id::new(7).unwrap()];
        let l = p.lines_en(ConsentAsk::Join, &names);
        assert!(l.contains(&"Audio is kept for 1 day.".to_owned()), "{l:?}");
        assert!(
            l.contains(
                &"Each person's words are transcribed; search and agents can read the transcript."
                    .to_owned()
            ),
            "{l:?}"
        );
        assert!(l.contains(&"Recorded now: Bob, 7.".to_owned()), "{l:?}");
        // A starter Sew sent without a display name is the host's name for them.
        p.started_by = Some(PromptMember {
            member_id: Id::new(BOB).unwrap(),
            display_name: String::new(),
        });
        assert_eq!(
            p.lines_en(ConsentAsk::Join, &names)[0],
            "Bob started recording this call."
        );
        // A DM call has no room name.
        p.room.name = None;
        p.room.kind = PromptRoomKind::Dm;
        assert_eq!(p.title_en(ConsentAsk::Join), "This call is being recorded");
    }
}
