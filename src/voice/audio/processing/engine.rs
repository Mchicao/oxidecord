use sonora::config::{
    AdaptiveDigital, EchoCanceller, GainController2, MaxProcessingRate, NoiseSuppression, Pipeline,
};
use sonora::{AudioProcessing, Config, StreamConfig};

use super::{
    Options,
    resample::{FRAME, RATE},
};
use crate::platform::audio::sanitize;

pub(super) struct Engine {
    apm: AudioProcessing,
    options: Options,
}

impl Engine {
    pub(super) fn new(options: Options) -> Self {
        Self {
            apm: AudioProcessing::builder()
                .config(Config {
                    pipeline: Pipeline {
                        maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz,
                        ..Pipeline::default()
                    },
                    echo_canceller: options.echo_cancellation.then(EchoCanceller::default),
                    noise_suppression: options.noise_suppression.then(NoiseSuppression::default),
                    gain_controller2: options.automatic_gain_control.then(|| GainController2 {
                        input_volume_controller: false,
                        adaptive_digital: Some(AdaptiveDigital {
                            max_gain_db: 30.,
                            initial_gain_db: 0.,
                            ..AdaptiveDigital::default()
                        }),
                        ..GainController2::default()
                    }),
                    ..Config::default()
                })
                .capture_config(StreamConfig::new(RATE, 1))
                .render_config(StreamConfig::new(RATE, 1))
                .build(),
            options,
        }
    }

    pub(super) fn render(&mut self, frame: &[f32; FRAME]) -> Result<(), &'static str> {
        if !self.options.echo_cancellation {
            return Ok(());
        }
        let mut unused = [0.; FRAME];
        self.apm
            .process_render_f32(&[&frame[..]], &mut [&mut unused[..]])
            .map_err(|_| "Sonora rejected the playback reference.")
    }

    pub(super) fn capture(
        &mut self,
        frame: &[f32; FRAME],
        output: &mut [f32; FRAME],
        delay_ms: i32,
    ) -> Result<(), &'static str> {
        if self.options.echo_cancellation {
            self.apm
                .set_stream_delay_ms(delay_ms)
                .map_err(|_| "Playback/capture delay is outside Sonora's supported range.")?;
        }
        self.apm
            .process_capture_f32(&[&frame[..]], &mut [&mut output[..]])
            .map_err(|_| "Sonora rejected the microphone frame.")?;
        for sample in output {
            *sample = sanitize(*sample);
        }
        Ok(())
    }
}
