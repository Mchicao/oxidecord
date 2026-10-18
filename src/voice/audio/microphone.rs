use std::io::Read as _;

use songbird::input::codecs::{get_codec_registry, get_probe};
use songbird::input::{AudioStream, Input, LiveInput, RawAdapter};
use symphonia::core::{
    errors::{Error, Result, SeekErrorKind},
    formats::{Cue, FormatOptions, FormatReader, Packet, SeekMode, SeekTo, SeekedTo, Track},
    io::{MediaSourceStream, MediaSourceStreamOptions},
    meta::{Metadata, MetadataLog},
};

use super::MicSource;

// RawAdapter's default MediaSourceStream prefetches microphone PCM across gate changes.
// This parsed source drains only the current 20 ms frame, immediately before decoding.
pub(super) fn microphone(source: MicSource, sample_rate: u32, channels: u32) -> Input {
    let header = RawAdapter::new(
        std::io::Cursor::new(Vec::<u8>::new()),
        sample_rate,
        channels,
    );
    let mut input = LiveInput::Raw(AudioStream {
        input: Box::new(header),
    })
    .promote(get_codec_registry(), get_probe())
    .expect("the built-in raw PCM header and decoder are supported");
    let parsed = input.parsed_mut().expect("promote creates a parsed input");
    parsed.format = Box::new(Microphone {
        source,
        track: parsed.format.tracks()[0].clone(),
        metadata: MetadataLog::default(),
        frame_bytes: (sample_rate / 50) as usize * channels as usize * size_of::<f32>(),
        frames: u64::from(sample_rate / 50),
        timestamp: 0,
    });
    parsed.supports_backseek = false;
    Input::Live(input, None)
}

struct Microphone {
    source: MicSource,
    track: Track,
    metadata: MetadataLog,
    frame_bytes: usize,
    frames: u64,
    timestamp: u64,
}

impl FormatReader for Microphone {
    fn try_new(_: MediaSourceStream, _: &FormatOptions) -> Result<Self> {
        Err(Error::Unsupported(
            "a microphone must be created from live capture",
        ))
    }

    fn cues(&self) -> &[Cue] {
        &[]
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn seek(&mut self, _: SeekMode, _: SeekTo) -> Result<SeekedTo> {
        Err(Error::SeekError(SeekErrorKind::Unseekable))
    }

    fn tracks(&self) -> &[Track] {
        std::slice::from_ref(&self.track)
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let mut data = vec![0u8; self.frame_bytes].into_boxed_slice();
        self.source.read_exact(&mut data)?;
        let packet = Packet::new_from_boxed_slice(0, self.timestamp, self.frames, data);
        self.timestamp += self.frames;
        Ok(packet)
    }

    fn into_inner(self: Box<Self>) -> MediaSourceStream {
        MediaSourceStream::new(Box::new(self.source), MediaSourceStreamOptions::default())
    }
}
