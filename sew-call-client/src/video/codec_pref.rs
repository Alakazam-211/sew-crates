//! The video codec preference and the receive codec set.
//!
//! **The preference** ([`VideoCodecPreference`]) is a per-device user setting: which codec a
//! camera or screen of this device publishes in. [`effective_publish_codec`] turns it and the
//! run-time H.264 detection ([`H264Detection`]) into the codec the publish side uses, with the
//! reason when "Prefer H.264" can't apply ([`CodecPreferenceReason`]). H.264 is only ever the
//! OS's own encoder, found at run time (`h264_encode_detection`, with `video-core`); nothing
//! assumes it from the platform, and nothing here ships or loads an H.264 codec.
//!
//! The engine keeps the setting ([`crate::CallClient::set_video_codec_preference`]) for every
//! call it makes, until the process ends. **Persisting it is the host's job**: read it back with
//! [`crate::CallClient::video_codec_preference`], store it with the app's settings, and set it
//! again at start.
//!
//! **The receive codec set** is what this device's media connection offers to receive:
//! VP8 always (kessel-video's pure-Rust decoder exists everywhere), and H.264 only when the
//! host declared it can decode it ([`crate::CallClient::set_video_decode`]: the OS decoders its
//! kessel-video backend lists and its webview's WebCodecs probe), never on Linux, and never
//! under [`VideoCodecPreference::PreferVp8`].

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::types::VideoCodec;

/// Which codec this device's camera and screen publish in. A per-device user setting; the
/// default is [`VideoCodecPreference::Automatic`].
///
/// Its JSON is `"automatic"`, `"prefer_h264"` or `"prefer_vp8"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum VideoCodecPreference {
    /// H.264 where this device's OS has a usable hardware H.264 encoder (found at run time),
    /// else VP8. An H.264 publication always carries a VP8 backup for subscribers that can't
    /// decode H.264.
    #[default]
    Automatic,
    /// H.264 when the run-time detection passes, as `Automatic`; otherwise VP8, and the
    /// readback says why ([`CodecPreferenceReason`]).
    PreferH264,
    /// VP8 always. This device also stops offering to receive H.264 (from its next join), so
    /// every sender reaches it in VP8.
    PreferVp8,
}

impl VideoCodecPreference {
    /// The setting's name: `automatic`, `prefer_h264` or `prefer_vp8`.
    pub fn as_str(self) -> &'static str {
        match self {
            VideoCodecPreference::Automatic => "automatic",
            VideoCodecPreference::PreferH264 => "prefer_h264",
            VideoCodecPreference::PreferVp8 => "prefer_vp8",
        }
    }

    /// The setting of a name [`VideoCodecPreference::as_str`] gives; `None` for any other.
    pub fn from_name(name: &str) -> Option<VideoCodecPreference> {
        match name {
            "automatic" => Some(VideoCodecPreference::Automatic),
            "prefer_h264" => Some(VideoCodecPreference::PreferH264),
            "prefer_vp8" => Some(VideoCodecPreference::PreferVp8),
            _ => None,
        }
    }
}

/// What the run-time check of this device's OS H.264 encoder found.
///
/// "Usable" means a hardware encoder: the backend's capabilities list an H.264 **encode** entry
/// run by hardware, the encoder opens with hardware only, and the opened session says it runs in
/// hardware. `h264_encode_detection` (with `video-core`) does the first check; the publish side
/// does the other two when the camera or screen turns on (`H264Detection::from_session`, with
/// `video-core`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum H264Detection {
    /// A hardware H.264 encoder is there (or opened and runs in hardware).
    Usable,
    /// The check ran and found no usable one: none listed, only a software one, or the
    /// hardware-only open failed.
    NotDetected,
    /// An encoder may be there, but the OS can't confirm it runs in hardware (an iOS before
    /// 17.4). Never treated as usable.
    Unconfirmed,
    /// This platform has no run-time check (yet): Linux (no OS H.264), Android and Windows
    /// (their backends would list an encoder without probing it). Never treated as usable.
    NoPlatformCheck,
}

impl H264Detection {
    /// Whether H.264 may be published.
    pub fn is_usable(self) -> bool {
        self == H264Detection::Usable
    }
}

/// Why "Prefer H.264" could not apply, so the device publishes VP8.
///
/// Its JSON is `"not_detected"`, `"platform"` or `"policy"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CodecPreferenceReason {
    /// The run-time check found no usable hardware H.264 encoder
    /// ([`H264Detection::NotDetected`]).
    NotDetected,
    /// This platform can't publish H.264 here: no run-time check on Linux, Android or Windows
    /// yet ([`H264Detection::NoPlatformCheck`]), or an iOS before 17.4, which can't confirm a
    /// hardware encoder ([`H264Detection::Unconfirmed`]).
    Platform,
    /// The call's media policy (the room's or the install's codecs) doesn't allow H.264
    /// ([`effective_publish_codec_under_policy`]).
    Policy,
}

impl CodecPreferenceReason {
    /// The reason's name: `not_detected`, `platform` or `policy`.
    pub fn as_str(self) -> &'static str {
        match self {
            CodecPreferenceReason::NotDetected => "not_detected",
            CodecPreferenceReason::Platform => "platform",
            CodecPreferenceReason::Policy => "policy",
        }
    }
}

/// The codec a camera or screen of this device publishes in under `pref`, given what the H.264
/// detection found, and why "Prefer H.264" didn't apply when it didn't.
///
/// | `pref` | `detected` | result |
/// |---|---|---|
/// | `PreferVp8` | any | `(Vp8, None)` |
/// | `Automatic` | `Usable` | `(H264, None)` |
/// | `Automatic` | otherwise | `(Vp8, None)` |
/// | `PreferH264` | `Usable` | `(H264, None)` |
/// | `PreferH264` | `NotDetected` | `(Vp8, Some(NotDetected))` |
/// | `PreferH264` | `Unconfirmed`, `NoPlatformCheck` | `(Vp8, Some(Platform))` |
///
/// `H264` means H.264 as the primary codec; the publish side still adds the VP8 backup. This is
/// the preference and the device only; in a call, use
/// [`effective_publish_codec_under_policy`], which also applies the media policy.
pub fn effective_publish_codec(
    pref: VideoCodecPreference,
    detected: H264Detection,
) -> (VideoCodec, Option<CodecPreferenceReason>) {
    match (pref, detected) {
        (VideoCodecPreference::PreferVp8, _) => (VideoCodec::Vp8, None),
        (_, H264Detection::Usable) => (VideoCodec::H264, None),
        (VideoCodecPreference::PreferH264, H264Detection::NotDetected) => {
            (VideoCodec::Vp8, Some(CodecPreferenceReason::NotDetected))
        }
        (VideoCodecPreference::PreferH264, _) => {
            (VideoCodec::Vp8, Some(CodecPreferenceReason::Platform))
        }
        _ => (VideoCodec::Vp8, None),
    }
}

/// [`effective_publish_codec`] in a call whose media policy allows H.264 (`h264_allowed`) or
/// not: without it, an `H264` turns into VP8, with [`CodecPreferenceReason::Policy`] under
/// "Prefer H.264". The device's own reasons come first (a device that can't publish H.264 says
/// so whatever the policy).
pub fn effective_publish_codec_under_policy(
    pref: VideoCodecPreference,
    detected: H264Detection,
    h264_allowed: bool,
) -> (VideoCodec, Option<CodecPreferenceReason>) {
    match effective_publish_codec(pref, detected) {
        (VideoCodec::H264, _) if !h264_allowed => {
            let reason =
                (pref == VideoCodecPreference::PreferH264).then_some(CodecPreferenceReason::Policy);
            (VideoCodec::Vp8, reason)
        }
        out => out,
    }
}

/// The video backends with no run-time H.264 check yet: always
/// [`H264Detection::NoPlatformCheck`]. Linux has no OS H.264; Android's and Windows' backends
/// would list one without probing it.
#[cfg(feature = "video-core")]
const NO_H264_BACKENDS: [&str; 4] = ["linux", "android", "windows", "null"];

/// The first of the three checks, from a kessel-video backend's capabilities: an H.264
/// **encode** entry run by hardware. A software entry never counts. Linux, Android and Windows
/// backends are always [`H264Detection::NoPlatformCheck`]. An iOS before 17.4 lists none, since
/// it can't confirm hardware: [`H264Detection::Unconfirmed`].
///
/// A `Usable` here can still turn into `NotDetected` or `Unconfirmed` when the encoder opens
/// (`H264Detection::from_session`, with `video-core`).
#[cfg(feature = "video-core")]
pub fn h264_encode_detection(caps: &kessel_video::Capabilities) -> H264Detection {
    use kessel_video::{CodecDirection, Implementation};
    if NO_H264_BACKENDS.contains(&caps.backend) {
        return H264Detection::NoPlatformCheck;
    }
    let hardware = caps.codecs.iter().any(|c| {
        c.codec == kessel_video::VideoCodec::H264
            && c.direction == CodecDirection::Encode
            && c.implementation == Implementation::Hardware
    });
    if hardware {
        H264Detection::Usable
    } else if ios_before_17_4(&caps.os) {
        H264Detection::Unconfirmed
    } else {
        H264Detection::NotDetected
    }
}

#[cfg(feature = "video-core")]
impl H264Detection {
    /// The last two checks, from an H.264 encoder opened with hardware only: what the session
    /// says runs it. Hardware is `Usable`; software is `NotDetected`; an OS that doesn't say
    /// (an iOS before 17.4) is `Unconfirmed`. A failed hardware-only open is `NotDetected`.
    pub fn from_session(implementation: kessel_video::Implementation) -> H264Detection {
        match implementation {
            kessel_video::Implementation::Hardware => H264Detection::Usable,
            kessel_video::Implementation::Software => H264Detection::NotDetected,
            _ => H264Detection::Unconfirmed,
        }
    }
}

/// Whether `os` (kessel-video's `Capabilities::os`, e.g. `iOS Version 17.2 (Build 21C62)`) is
/// an iOS before 17.4. Anything it can't read is `false`.
#[cfg(feature = "video-core")]
fn ios_before_17_4(os: &str) -> bool {
    let Some(rest) = os.strip_prefix("iOS") else {
        return false;
    };
    let Some(version) = rest
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find(|t| t.contains(|c: char| c.is_ascii_digit()))
    else {
        return false;
    };
    let mut parts = version.split('.').map(|p| p.parse::<u32>().ok());
    match (parts.next().flatten(), parts.next().flatten().unwrap_or(0)) {
        (Some(major), minor) => (major, minor) < (17, 4),
        _ => false,
    }
}

/// The video codecs a media connection offers to receive: VP8 always, H.264 when `h264`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CodecSet {
    h264: bool,
}

impl CodecSet {
    /// VP8 only.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const VP8_ONLY: CodecSet = CodecSet { h264: false };
    /// VP8 and H.264.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const ALL: CodecSet = CodecSet { h264: true };

    /// Whether `codec` is in the set.
    pub(crate) fn contains(self, codec: VideoCodec) -> bool {
        match codec {
            VideoCodec::Vp8 => true,
            VideoCodec::H264 => self.h264,
        }
    }

    /// The codecs, VP8 first.
    pub(crate) fn codecs(self) -> Vec<VideoCodec> {
        let mut v = vec![VideoCodec::Vp8];
        if self.h264 {
            v.push(VideoCodec::H264);
        }
        v
    }

    /// The set a call's media connection offers:
    /// - VP8 always;
    /// - H.264 only with `cfg_h264` ([`crate::ClientConfig::video_h264`]), and not under
    ///   [`VideoCodecPreference::PreferVp8`], and, once the host declared what it decodes
    ///   (`declared`), only if that lists H.264 and never on Linux.
    ///
    /// A host that never declared keeps `cfg_h264` alone (the behaviour before the
    /// declaration existed).
    pub(crate) fn negotiated(
        cfg_h264: bool,
        declared: Option<&[VideoCodec]>,
        pref: VideoCodecPreference,
    ) -> CodecSet {
        let declared_ok = match declared {
            None => true,
            Some(d) => !cfg!(target_os = "linux") && d.contains(&VideoCodec::H264),
        };
        CodecSet {
            h264: cfg_h264 && pref != VideoCodecPreference::PreferVp8 && declared_ok,
        }
    }
}

/// The engine's video codec settings, shared by the [`crate::CallClient`] and its calls: the
/// preference and what the host declared it decodes.
#[derive(Debug, Default)]
pub(crate) struct CodecSettings {
    pref: Mutex<VideoCodecPreference>,
    decode: Mutex<Option<Vec<VideoCodec>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl CodecSettings {
    pub(crate) fn preference(&self) -> VideoCodecPreference {
        *lock(&self.pref)
    }

    pub(crate) fn set_preference(&self, pref: VideoCodecPreference) {
        *lock(&self.pref) = pref;
    }

    pub(crate) fn decode(&self) -> Option<Vec<VideoCodec>> {
        lock(&self.decode).clone()
    }

    pub(crate) fn set_decode(&self, codecs: &[VideoCodec]) {
        let mut v = codecs.to_vec();
        v.sort_by_key(|c| c.as_str());
        v.dedup();
        *lock(&self.decode) = Some(v);
    }

    /// The set a call joined now offers ([`CodecSet::negotiated`]).
    pub(crate) fn negotiated(&self, cfg_h264: bool) -> CodecSet {
        CodecSet::negotiated(cfg_h264, self.decode().as_deref(), self.preference())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFS: [VideoCodecPreference; 3] = [
        VideoCodecPreference::Automatic,
        VideoCodecPreference::PreferH264,
        VideoCodecPreference::PreferVp8,
    ];
    const DETECTIONS: [H264Detection; 4] = [
        H264Detection::Usable,
        H264Detection::NotDetected,
        H264Detection::Unconfirmed,
        H264Detection::NoPlatformCheck,
    ];

    #[test]
    fn the_effective_codec_follows_the_table() {
        use CodecPreferenceReason as R;
        use H264Detection as D;
        use VideoCodec::{H264, Vp8};
        use VideoCodecPreference as P;
        let want = [
            (P::Automatic, D::Usable, (H264, None)),
            (P::Automatic, D::NotDetected, (Vp8, None)),
            (P::Automatic, D::Unconfirmed, (Vp8, None)),
            (P::Automatic, D::NoPlatformCheck, (Vp8, None)),
            (P::PreferH264, D::Usable, (H264, None)),
            (P::PreferH264, D::NotDetected, (Vp8, Some(R::NotDetected))),
            (P::PreferH264, D::Unconfirmed, (Vp8, Some(R::Platform))),
            (P::PreferH264, D::NoPlatformCheck, (Vp8, Some(R::Platform))),
            (P::PreferVp8, D::Usable, (Vp8, None)),
            (P::PreferVp8, D::NotDetected, (Vp8, None)),
            (P::PreferVp8, D::Unconfirmed, (Vp8, None)),
            (P::PreferVp8, D::NoPlatformCheck, (Vp8, None)),
        ];
        assert_eq!(want.len(), PREFS.len() * DETECTIONS.len());
        for (p, d, out) in want {
            assert_eq!(effective_publish_codec(p, d), out, "{p:?} {d:?}");
            // A policy that allows H.264 changes nothing.
            assert_eq!(effective_publish_codec_under_policy(p, d, true), out);
        }
        // A policy without H.264: VP8, and Prefer H.264 says `policy`.
        assert_eq!(
            effective_publish_codec_under_policy(P::PreferH264, D::Usable, false),
            (Vp8, Some(R::Policy))
        );
        assert_eq!(
            effective_publish_codec_under_policy(P::Automatic, D::Usable, false),
            (Vp8, None)
        );
        // The device's own reason first.
        assert_eq!(
            effective_publish_codec_under_policy(P::PreferH264, D::NoPlatformCheck, false),
            (Vp8, Some(R::Platform))
        );
    }

    #[test]
    fn h264_is_published_only_when_detected() {
        for p in PREFS {
            for d in DETECTIONS {
                let (codec, _) = effective_publish_codec(p, d);
                if codec == VideoCodec::H264 {
                    assert!(d.is_usable(), "{p:?} {d:?}");
                    assert_ne!(p, VideoCodecPreference::PreferVp8);
                }
            }
        }
    }

    #[test]
    fn the_preference_names_round_trip() {
        for p in PREFS {
            assert_eq!(VideoCodecPreference::from_name(p.as_str()), Some(p));
            let json = serde_json::to_string(&p).unwrap_or_default();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
        }
        assert_eq!(VideoCodecPreference::from_name("h264"), None);
        assert_eq!(
            VideoCodecPreference::default(),
            VideoCodecPreference::Automatic
        );
        for r in [
            CodecPreferenceReason::NotDetected,
            CodecPreferenceReason::Platform,
            CodecPreferenceReason::Policy,
        ] {
            let json = serde_json::to_string(&r).unwrap_or_default();
            assert_eq!(json, format!("\"{}\"", r.as_str()));
        }
    }

    #[cfg(feature = "video-core")]
    #[test]
    fn ios_versions() {
        assert!(ios_before_17_4("iOS Version 17.2 (Build 21C62)"));
        assert!(ios_before_17_4("iOS Version 16.7.8 (Build 20H343)"));
        assert!(ios_before_17_4("iOS 17"));
        assert!(!ios_before_17_4("iOS Version 17.4 (Build 21E219)"));
        assert!(!ios_before_17_4("iOS Version 18.1 (Build 22B83)"));
        assert!(!ios_before_17_4("macOS Version 14.2 (Build 23C64)"));
        assert!(!ios_before_17_4("iOS"));
        assert!(!ios_before_17_4("fake"));
    }

    #[test]
    fn prefer_vp8_and_a_declaration_without_h264_offer_vp8_only() {
        use VideoCodec::{H264, Vp8};
        for p in PREFS {
            assert_eq!(
                CodecSet::negotiated(false, None, p),
                CodecSet::VP8_ONLY,
                "{p:?}"
            );
            assert_eq!(
                CodecSet::negotiated(true, Some(&[Vp8][..]), p),
                CodecSet::VP8_ONLY
            );
            assert_eq!(
                CodecSet::negotiated(true, Some(&[][..]), p),
                CodecSet::VP8_ONLY
            );
        }
        for declared in [None, Some(&[Vp8, H264][..])] {
            assert_eq!(
                CodecSet::negotiated(true, declared, VideoCodecPreference::PreferVp8),
                CodecSet::VP8_ONLY
            );
        }
        // Before any declaration, the config alone decides (0.5.0's behaviour).
        assert_eq!(
            CodecSet::negotiated(true, None, VideoCodecPreference::Automatic),
            CodecSet::ALL
        );
        assert!(CodecSet::VP8_ONLY.contains(Vp8));
        assert!(!CodecSet::VP8_ONLY.contains(H264));
        assert_eq!(CodecSet::ALL.codecs(), vec![Vp8, H264]);
    }

    /// Linux: a declared decode set never brings H.264 in, whatever it lists.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_never_offers_h264_once_declared() {
        use VideoCodec::{H264, Vp8};
        for p in PREFS {
            for cfg_h264 in [false, true] {
                for declared in [&[][..], &[Vp8][..], &[H264][..], &[Vp8, H264][..]] {
                    let set = CodecSet::negotiated(cfg_h264, Some(declared), p);
                    assert!(!set.contains(H264), "{p:?} {cfg_h264} {declared:?}");
                    assert!(set.contains(Vp8));
                }
            }
            // The default config (no H.264 on Linux) without a declaration: VP8 only too.
            let cfg = crate::ClientConfig::new("t", std::env::temp_dir());
            assert!(!CodecSet::negotiated(cfg.video_h264, None, p).contains(H264));
        }
        let s = CodecSettings::default();
        s.set_decode(&[H264, Vp8, H264]);
        assert_eq!(s.decode(), Some(vec![H264, Vp8]));
        assert!(!s.negotiated(true).contains(H264));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_declared_h264_decoder_is_offered() {
        use VideoCodec::{H264, Vp8};
        let s = CodecSettings::default();
        assert_eq!(s.negotiated(true), CodecSet::ALL);
        s.set_decode(&[Vp8]);
        assert_eq!(s.negotiated(true), CodecSet::VP8_ONLY);
        s.set_decode(&[Vp8, H264]);
        assert_eq!(s.negotiated(true), CodecSet::ALL);
        assert_eq!(s.negotiated(false), CodecSet::VP8_ONLY);
        s.set_preference(VideoCodecPreference::PreferVp8);
        assert_eq!(s.negotiated(true), CodecSet::VP8_ONLY);
    }

    #[cfg(feature = "video-core")]
    #[test]
    fn detection_from_capabilities() {
        use kessel_video::{Capabilities, CodecCapability, CodecDirection, Implementation};
        let entry = |dir, imp| {
            CodecCapability::new(
                kessel_video::VideoCodec::H264,
                dir,
                imp,
                "t",
                1920,
                1080,
                30,
            )
        };
        let caps = |backend: &'static str, os: &str, entries: Vec<CodecCapability>| {
            let mut c = Capabilities::new(backend, os);
            c.codecs = entries;
            c
        };
        let hw = || entry(CodecDirection::Encode, Implementation::Hardware);
        let sw = || entry(CodecDirection::Encode, Implementation::Software);
        let dec = || entry(CodecDirection::Decode, Implementation::Hardware);
        let mac = "macOS Version 14.2 (Build 23C64)";
        assert_eq!(
            h264_encode_detection(&caps("apple", mac, vec![hw(), sw(), dec()])),
            H264Detection::Usable
        );
        // Software only (a Mac VM): not usable; a decoder alone isn't an encoder.
        assert_eq!(
            h264_encode_detection(&caps("apple", mac, vec![sw(), dec()])),
            H264Detection::NotDetected
        );
        assert_eq!(
            h264_encode_detection(&caps(
                "apple",
                "iOS Version 17.2 (Build 21C62)",
                vec![dec()]
            )),
            H264Detection::Unconfirmed
        );
        assert_eq!(
            h264_encode_detection(&caps(
                "apple",
                "iOS Version 17.5 (Build 21F79)",
                vec![dec()]
            )),
            H264Detection::NotDetected
        );
        // No run-time check on these yet, whatever they list.
        for b in NO_H264_BACKENDS {
            assert_eq!(
                h264_encode_detection(&caps(b, "x", vec![hw()])),
                H264Detection::NoPlatformCheck,
                "{b}"
            );
        }
        // A test backend's hardware entry counts (the lanes' fixture).
        assert_eq!(
            h264_encode_detection(&caps("fixture", "fixture", vec![hw()])),
            H264Detection::Usable
        );
        assert_eq!(
            H264Detection::from_session(Implementation::Hardware),
            H264Detection::Usable
        );
        assert_eq!(
            H264Detection::from_session(Implementation::Software),
            H264Detection::NotDetected
        );
        assert_eq!(
            H264Detection::from_session(Implementation::Unknown),
            H264Detection::Unconfirmed
        );
    }
}
