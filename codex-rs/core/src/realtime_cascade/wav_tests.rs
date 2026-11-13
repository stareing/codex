use super::*;
use pretty_assertions::assert_eq;

fn wav_with(
    tag: u16,
    channels: u16,
    rate: u32,
    bits: u16,
    data: &[u8],
    extensible: bool,
) -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&(if extensible { FORMAT_EXTENSIBLE } else { tag }).to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    let block = channels * bits / 8;
    fmt.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    fmt.extend_from_slice(&block.to_le_bytes());
    fmt.extend_from_slice(&bits.to_le_bytes());
    if extensible {
        fmt.extend_from_slice(&22u16.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());
        fmt.extend_from_slice(&0u32.to_le_bytes());
        fmt.extend_from_slice(&tag.to_le_bytes());
        fmt.extend_from_slice(&[0u8; 14]);
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    // An odd-sized chunk before fmt checks the padding rule.
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&[1, 2, 3, 0]);
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    out.extend_from_slice(&fmt);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

#[test]
fn encode_then_decode_round_trips() {
    let samples = vec![0, 1, -1, i16::MAX, i16::MIN, 1234];
    let wav = encode_pcm16_mono(16_000, &samples);
    assert_eq!(wav.len(), 44 + samples.len() * 2);
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(
        u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]),
        36 + 12
    );
    assert_eq!(
        u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
        16_000
    );
    assert_eq!(
        u32::from_le_bytes([wav[28], wav[29], wav[30], wav[31]]),
        32_000
    );
    assert_eq!(
        decode(&wav).unwrap(),
        DecodedWav {
            sample_rate: 16_000,
            channels: 1,
            samples
        }
    );
}

/// Format tag, bits per sample, data bytes, extensible header, expected samples.
type FormatCase<'a> = (u16, u16, &'a [u8], bool, Vec<i16>);

#[test]
fn decodes_every_supported_sample_format() {
    // Each case encodes [0.5 full scale, -0.5 full scale] in stereo-free mono.
    let pcm16 = [0x00, 0x40, 0x00, 0xC0];
    let pcm24 = [0x00, 0x00, 0x40, 0x00, 0x00, 0xC0];
    let pcm32 = [0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0xC0];
    let mut float = Vec::new();
    float.extend_from_slice(&0.5f32.to_le_bytes());
    float.extend_from_slice(&(-0.5f32).to_le_bytes());
    let cases: Vec<FormatCase<'_>> = vec![
        (FORMAT_PCM, 16, &pcm16, false, vec![16384, -16384]),
        (FORMAT_PCM, 24, &pcm24, false, vec![16384, -16384]),
        (FORMAT_PCM, 32, &pcm32, false, vec![16384, -16384]),
        (FORMAT_FLOAT, 32, &float, false, vec![16384, -16384]),
        (FORMAT_PCM, 16, &pcm16, true, vec![16384, -16384]),
    ];
    for (tag, bits, data, extensible, expected) in cases {
        let wav = wav_with(tag, 1, 22_050, bits, data, extensible);
        let decoded = decode(&wav).unwrap_or_else(|e| panic!("{tag}/{bits}: {e}"));
        assert_eq!(decoded.samples, expected, "{tag}/{bits}/{extensible}");
        assert_eq!(decoded.sample_rate, 22_050);
    }
}

#[test]
fn rejects_what_it_cannot_play() {
    assert_eq!(decode(b"not a wav at all"), Err(WavError::NotWav));
    assert_eq!(decode(b"RIFF\0\0\0\0WAVE"), Err(WavError::Truncated));
    let cases = [
        wav_with(FORMAT_PCM, 3, 24_000, 16, &[0; 6], false),
        wav_with(FORMAT_PCM, 1, 96_000, 16, &[0; 2], false),
        wav_with(FORMAT_PCM, 1, 24_000, 8, &[0; 2], false),
        wav_with(7, 1, 24_000, 16, &[0; 2], false),
    ];
    for wav in cases {
        assert!(matches!(decode(&wav), Err(WavError::Unsupported(_))));
    }
}

#[test]
fn a_streamed_data_size_reads_to_the_end() {
    let mut wav = encode_pcm16_mono(24_000, &[5, 6, 7]);
    wav[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(decode(&wav).unwrap().samples, vec![5, 6, 7]);
}

#[test]
fn downmix_averages_channels() {
    assert_eq!(downmix_to_mono(&[100, 300, -10, 10], 2), vec![200, 0]);
    assert_eq!(downmix_to_mono(&[1, 2, 3], 1), vec![1, 2, 3]);
}

#[test]
fn resampling_changes_length_and_interpolates() {
    assert_eq!(
        resample_linear(&[0, 100, 200, 300], 8_000, 8_000),
        vec![0, 100, 200, 300]
    );
    // Upsampling by 2 places a midpoint between neighbours.
    assert_eq!(
        resample_linear(&[0, 100, 200, 300], 12_000, 24_000),
        vec![0, 50, 100, 150, 200, 250, 300, 300]
    );
    // Downsampling by 2 keeps every other sample.
    assert_eq!(
        resample_linear(&[0, 1, 2, 3, 4, 5], 48_000, 24_000),
        vec![0, 2, 4]
    );
    let one_second = vec![0i16; 22_050];
    assert_eq!(
        resample_linear(&one_second, 22_050, OUTPUT_SAMPLE_RATE).len(),
        24_000
    );
}

#[test]
fn base64_pcm_round_trips_and_rejects_odd_bytes() {
    let samples = vec![1, -2, 300, i16::MIN];
    assert_eq!(
        pcm16_from_base64(&pcm16_to_base64(&samples)).unwrap(),
        samples
    );
    assert!(pcm16_from_base64("AA==").is_err());
    assert!(pcm16_from_base64("not base64!").is_err());
}

#[test]
fn synthesized_audio_becomes_output_pcm() {
    // Stereo 12 kHz → mono 24 kHz: twice as many frames as input frames.
    let stereo: Vec<i16> = vec![100, 300, 100, 300, 100, 300];
    let mut data = Vec::new();
    for s in &stereo {
        data.extend_from_slice(&s.to_le_bytes());
    }
    let wav = wav_with(FORMAT_PCM, 2, 12_000, 16, &data, false);
    assert_eq!(to_output_pcm(&wav).unwrap(), vec![200; 6]);
}
