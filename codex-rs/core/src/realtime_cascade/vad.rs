//! Deterministic energy-based voice activity detection: input PCM is cut into [`VAD_FRAME_MS`]
//! frames, a frame is voiced when its RMS reaches [`SPEECH_RMS_THRESHOLD`], speech starts after
//! [`SPEECH_START_FRAMES`] voiced frames in a row and an utterance ends after [`HANG_MS`] of
//! unvoiced frames. Memory is bounded: at most one partial frame, the pre-roll and one utterance
//! of [`MAX_UTTERANCE_MS`].

use std::collections::VecDeque;

/// Analysis frame length.
pub(crate) const VAD_FRAME_MS: u32 = 20;
/// RMS (16-bit full scale = 32767) at or above which a frame counts as voiced; about -35 dBFS.
pub(crate) const SPEECH_RMS_THRESHOLD: f64 = 600.0;
/// Voiced frames in a row that start an utterance (60 ms): clicks and pops stay below it.
pub(crate) const SPEECH_START_FRAMES: u32 = 3;
/// Unvoiced time that ends an utterance.
pub(crate) const HANG_MS: u32 = 700;
/// Utterances with less voiced time than this are dropped (coughs, door slams).
pub(crate) const MIN_VOICED_MS: u32 = 200;
/// An utterance is cut here and a new one continues without another start event.
pub(crate) const MAX_UTTERANCE_MS: u32 = 30_000;
/// Audio kept from before the start so the first syllable is not clipped.
pub(crate) const PRE_ROLL_MS: u32 = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VadEvent {
    /// The user started speaking (barge-in point).
    SpeechStarted,
    /// One finished utterance, mono samples at the detector's rate.
    Utterance(Vec<i16>),
}

#[derive(Debug)]
enum State {
    Idle {
        pre_roll: VecDeque<i16>,
        voiced_run: u32,
    },
    Speaking {
        samples: Vec<i16>,
        silent_frames: u32,
        voiced_frames: u32,
    },
}

#[derive(Debug)]
pub(crate) struct EnergyVad {
    sample_rate: u32,
    frame_len: usize,
    partial: Vec<i16>,
    state: State,
}

fn samples_for_ms(sample_rate: u32, ms: u32) -> usize {
    (u64::from(sample_rate) * u64::from(ms) / 1000) as usize
}

fn frame_rms(frame: &[i16]) -> f64 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / frame.len() as f64).sqrt()
}

impl EnergyVad {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let frame_len = samples_for_ms(sample_rate, VAD_FRAME_MS).max(1);
        Self {
            sample_rate,
            frame_len,
            partial: Vec::with_capacity(frame_len),
            state: Self::idle(),
        }
    }

    fn idle() -> State {
        State::Idle {
            pre_roll: VecDeque::new(),
            voiced_run: 0,
        }
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Feeds mono samples; returns the events of every frame they completed, in order.
    pub(crate) fn push(&mut self, samples: &[i16]) -> Vec<VadEvent> {
        let mut events = Vec::new();
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (self.frame_len - self.partial.len()).min(rest.len());
            self.partial.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.partial.len() == self.frame_len {
                let frame = std::mem::take(&mut self.partial);
                self.on_frame(&frame, &mut events);
                self.partial = frame;
                self.partial.clear();
            }
        }
        events
    }

    fn on_frame(&mut self, frame: &[i16], events: &mut Vec<VadEvent>) {
        let voiced = frame_rms(frame) >= SPEECH_RMS_THRESHOLD;
        let pre_roll_cap = samples_for_ms(self.sample_rate, PRE_ROLL_MS)
            + self.frame_len * SPEECH_START_FRAMES as usize;
        let max_utterance = samples_for_ms(self.sample_rate, MAX_UTTERANCE_MS);
        let hang_frames = HANG_MS.div_ceil(VAD_FRAME_MS);
        let min_voiced_frames = MIN_VOICED_MS.div_ceil(VAD_FRAME_MS);
        match &mut self.state {
            State::Idle {
                pre_roll,
                voiced_run,
            } => {
                pre_roll.extend(frame.iter().copied());
                while pre_roll.len() > pre_roll_cap {
                    pre_roll.pop_front();
                }
                *voiced_run = if voiced { *voiced_run + 1 } else { 0 };
                if *voiced_run >= SPEECH_START_FRAMES {
                    let samples: Vec<i16> = pre_roll.drain(..).collect();
                    self.state = State::Speaking {
                        samples,
                        silent_frames: 0,
                        voiced_frames: SPEECH_START_FRAMES,
                    };
                    events.push(VadEvent::SpeechStarted);
                }
            }
            State::Speaking {
                samples,
                silent_frames,
                voiced_frames,
            } => {
                samples.extend_from_slice(frame);
                if voiced {
                    *silent_frames = 0;
                    *voiced_frames += 1;
                } else {
                    *silent_frames += 1;
                }
                if *silent_frames >= hang_frames {
                    // Drop the trailing silence that ended the utterance.
                    let keep = samples
                        .len()
                        .saturating_sub(*silent_frames as usize * self.frame_len);
                    samples.truncate(keep);
                    let finished = std::mem::take(samples);
                    if *voiced_frames >= min_voiced_frames {
                        events.push(VadEvent::Utterance(finished));
                    }
                    self.state = Self::idle();
                } else if samples.len() >= max_utterance {
                    events.push(VadEvent::Utterance(std::mem::take(samples)));
                    *silent_frames = 0;
                    *voiced_frames = 0;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "vad_tests.rs"]
mod tests;
