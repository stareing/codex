use super::*;
use pretty_assertions::assert_eq;

const RATE: u32 = 24_000;

/// A square wave of the given amplitude: its RMS equals the amplitude.
fn tone(ms: u32, amplitude: i16) -> Vec<i16> {
    (0..samples_for_ms(RATE, ms))
        .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
        .collect()
}

fn silence(ms: u32) -> Vec<i16> {
    vec![0; samples_for_ms(RATE, ms)]
}

fn utterances(events: &[VadEvent]) -> Vec<usize> {
    events
        .iter()
        .filter_map(|event| match event {
            VadEvent::Utterance(samples) => Some(samples.len()),
            VadEvent::SpeechStarted => None,
        })
        .collect()
}

fn starts(events: &[VadEvent]) -> usize {
    events
        .iter()
        .filter(|event| **event == VadEvent::SpeechStarted)
        .count()
}

#[test]
fn constants_keep_their_documented_values() {
    assert_eq!(VAD_FRAME_MS, 20);
    assert_eq!(SPEECH_RMS_THRESHOLD, 600.0);
    assert_eq!(SPEECH_START_FRAMES, 3);
    assert_eq!(HANG_MS, 700);
    assert_eq!(MIN_VOICED_MS, 200);
    assert_eq!(MAX_UTTERANCE_MS, 30_000);
    assert_eq!(PRE_ROLL_MS, 200);
}

#[test]
fn threshold_is_inclusive_and_quiet_audio_never_starts() {
    let mut vad = EnergyVad::new(RATE);
    assert!(vad.push(&tone(2_000, 599)).is_empty());
    assert!(vad.push(&silence(2_000)).is_empty());
    let events = vad.push(&tone(60, 600));
    assert_eq!(events, vec![VadEvent::SpeechStarted]);
}

#[test]
fn speech_starts_after_three_voiced_frames_only() {
    let mut vad = EnergyVad::new(RATE);
    // Two voiced frames then silence: a click, no start.
    assert!(vad.push(&tone(40, 5_000)).is_empty());
    assert!(vad.push(&silence(20)).is_empty());
    assert!(vad.push(&tone(40, 5_000)).is_empty());
    assert_eq!(vad.push(&tone(20, 5_000)), vec![VadEvent::SpeechStarted]);
}

#[test]
fn utterance_ends_exactly_after_the_hang_time_and_drops_trailing_silence() {
    let mut vad = EnergyVad::new(RATE);
    let mut events = vad.push(&silence(100));
    events.extend(vad.push(&tone(500, 5_000)));
    assert_eq!(starts(&events), 1);
    // One frame short of the hang time: still speaking.
    assert!(utterances(&vad.push(&silence(HANG_MS - VAD_FRAME_MS))).is_empty());
    let events = vad.push(&silence(VAD_FRAME_MS));
    // 500 ms of speech plus the 100 ms of pre-roll before it; the silence is cut off.
    assert_eq!(utterances(&events), vec![samples_for_ms(RATE, 600)]);
}

#[test]
fn short_pauses_do_not_split_an_utterance() {
    let mut vad = EnergyVad::new(RATE);
    let mut events = vad.push(&tone(300, 5_000));
    events.extend(vad.push(&silence(HANG_MS - VAD_FRAME_MS)));
    events.extend(vad.push(&tone(300, 5_000)));
    events.extend(vad.push(&silence(HANG_MS)));
    assert_eq!(starts(&events), 1);
    assert_eq!(utterances(&events).len(), 1);
}

#[test]
fn too_little_voiced_time_is_dropped() {
    let mut vad = EnergyVad::new(RATE);
    let mut events = vad.push(&tone(MIN_VOICED_MS - VAD_FRAME_MS, 5_000));
    events.extend(vad.push(&silence(HANG_MS)));
    assert_eq!(starts(&events), 1);
    assert!(utterances(&events).is_empty());

    let mut events = vad.push(&tone(MIN_VOICED_MS, 5_000));
    events.extend(vad.push(&silence(HANG_MS)));
    assert_eq!(utterances(&events).len(), 1);
}

#[test]
fn pre_roll_is_bounded() {
    let mut vad = EnergyVad::new(RATE);
    let mut events = vad.push(&silence(5_000));
    events.extend(vad.push(&tone(MIN_VOICED_MS, 5_000)));
    events.extend(vad.push(&silence(HANG_MS)));
    // Pre-roll (200 ms) + speech; the 5 s of silence before is not kept.
    assert_eq!(
        utterances(&events),
        vec![samples_for_ms(RATE, PRE_ROLL_MS + MIN_VOICED_MS)]
    );
}

#[test]
fn long_speech_is_cut_at_the_maximum_without_a_second_start() {
    let mut vad = EnergyVad::new(RATE);
    let mut events = vad.push(&tone(MAX_UTTERANCE_MS + 1_000, 5_000));
    events.extend(vad.push(&silence(HANG_MS)));
    assert_eq!(starts(&events), 1);
    let lengths = utterances(&events);
    assert_eq!(lengths.len(), 2);
    assert_eq!(lengths[0], samples_for_ms(RATE, MAX_UTTERANCE_MS));
    assert!(
        lengths
            .iter()
            .all(|&len| len <= samples_for_ms(RATE, MAX_UTTERANCE_MS))
    );
}

#[test]
fn chunking_does_not_change_the_result() {
    let mut audio = silence(100);
    audio.extend(tone(450, 4_000));
    audio.extend(silence(HANG_MS + 100));
    let mut whole = EnergyVad::new(RATE);
    let expected = whole.push(&audio);
    let mut chunked = EnergyVad::new(RATE);
    let mut events = Vec::new();
    for chunk in audio.chunks(137) {
        events.extend(chunked.push(chunk));
    }
    assert_eq!(events, expected);
    assert_eq!(utterances(&events).len(), 1);
}
