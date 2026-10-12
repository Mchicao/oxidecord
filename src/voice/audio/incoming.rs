//! Decoding one sender's packets, and concealing the ones that were lost.

use std::sync::Arc;

use opus2::Decoder;
use songbird::constants::{MONO_FRAME_SIZE, SAMPLE_RATE};

use super::CHANNELS;
use super::ring::SampleRing;

/// Opus's longest frame, 120ms, in samples per channel.
const MAX_FRAME: usize = 5760;

/// How much a sender's ring builds up before it starts draining again.
///
/// Packets are played straight off the network, 20ms apart give or take
/// however much the route smears them, so a reader that drains to empty
/// underruns on nearly every callback. The gaps that leaves repeat at the
/// callback rate, which is heard as a harsh buzz rather than as the dropouts
/// they are; a buffer this deep costs latency once and stops it.
const PLAYBACK_PRIME_MS: u32 = 60;

/// The most lost packets in a row that get concealed. A longer run is a
/// sender that dropped out rather than a network that lost a few, and
/// papering over it would only hold up the audio that comes next.
const MAX_CONCEALED: i16 = 5;

/// How far behind the expected sequence number a packet can be and still be
/// taken for one that arrived late. Further back than this is a sender that
/// started its count over.
const REORDER_WINDOW: i16 = 32;

/// One sender's packets on their way to the mixer.
pub(super) struct Incoming {
    decoder: Decoder,
    pub(super) ring: Arc<SampleRing>,
    /// The sequence number the next packet should carry, once there has been
    /// a first one to count from.
    expected: Option<u16>,
    /// Samples per channel in the last frame decoded: how much a concealed
    /// frame has to cover, since there's no packet to say.
    frame: usize,
    decoded: Vec<f32>,
}

impl Incoming {
    pub(super) fn new() -> Option<Self> {
        Some(Self {
            decoder: Decoder::new(SAMPLE_RATE, opus2::Channels::Stereo).ok()?,
            ring: Arc::new(SampleRing::new(SAMPLE_RATE, CHANNELS, PLAYBACK_PRIME_MS)),
            expected: None,
            frame: MONO_FRAME_SIZE,
            decoded: vec![0.; MAX_FRAME * CHANNELS],
        })
    }

    /// Decodes a packet into the ring, concealing whatever was lost before it.
    ///
    /// Packets are played in the order they arrive rather than held back to
    /// be sorted: reordering is rare enough on a voice route that waiting on
    /// it would cost every packet latency to save the odd one. A packet that
    /// turns up after its moment has passed has already been concealed, so
    /// it's dropped.
    pub(super) fn receive(&mut self, sequence: u16, opus: &[u8]) {
        let gap = self
            .expected
            .map_or(0, |expected| sequence.wrapping_sub(expected) as i16);
        if (-REORDER_WINDOW..0).contains(&gap) {
            return;
        }
        self.expected = Some(sequence.wrapping_add(1));

        if (1..=MAX_CONCEALED).contains(&gap) {
            for _ in 1..gap {
                self.decode(&[], false);
            }
            // A packet can carry a rougher copy of the one before it, which
            // decoding with FEC recovers. Without one, Opus conceals instead.
            self.decode(opus, true);
        }
        self.decode(opus, false);
    }

    fn decode(&mut self, opus: &[u8], fec: bool) {
        // Concealment and recovery fill exactly the room they're given, which
        // has to be a whole frame; a real packet says how long it is itself.
        let room = if opus.is_empty() || fec {
            self.frame * CHANNELS
        } else {
            self.decoded.len()
        };
        let Ok(frames) = self
            .decoder
            .decode_float(opus, &mut self.decoded[..room], fec)
        else {
            return;
        };
        if !opus.is_empty() && !fec {
            self.frame = frames;
        }
        self.ring
            .push(self.decoded[..frames * CHANNELS].iter().copied());
    }
}
