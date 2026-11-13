//! PCM and WAV conversions of the cascade voice mode: input chunks arrive as base64 PCM16 LE,
//! utterances leave as WAV, synthesized WAV comes back in whatever format the provider chose and
//! leaves as base64 PCM16 LE mono at [`OUTPUT_SAMPLE_RATE`].

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

/// Sample rate of `outputAudio/delta` chunks (the realtime protocol's PCM format).
pub(crate) const OUTPUT_SAMPLE_RATE: u32 = 24_000;
/// Accepted input and decoded sample rates.
pub(crate) const MIN_SAMPLE_RATE: u32 = 8_000;
pub(crate) const MAX_SAMPLE_RATE: u32 = 48_000;
/// Accepted channel counts of input chunks and decoded WAV.
pub(crate) const MAX_CHANNELS: u16 = 2;

const WAV_HEADER_BYTES: usize = 44;
const FORMAT_PCM: u16 = 1;
const FORMAT_FLOAT: u16 = 3;
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WavError {
    NotWav,
    Unsupported(String),
    Truncated,
}

impl std::fmt::Display for WavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WavError::NotWav => write!(f, "the speech response is not a WAV file"),
            WavError::Unsupported(what) => write!(f, "unsupported WAV format: {what}"),
            WavError::Truncated => write!(f, "the WAV file is truncated"),
        }
    }
}

/// A decoded WAV file: interleaved 16-bit samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedWav {
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    pub(crate) samples: Vec<i16>,
}

/// A canonical 44-byte-header WAV of mono 16-bit PCM.
pub(crate) fn encode_pcm16_mono(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(WAV_HEADER_BYTES + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&FORMAT_PCM.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Decodes PCM 16/24/32-bit integer and 32-bit float WAV (plain or extensible) to 16-bit samples.
/// A `data` chunk longer than the file (streamed WAV writers put 0xFFFFFFFF there) is read to the end.
pub(crate) fn decode(bytes: &[u8]) -> Result<DecodedWav, WavError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWav);
    }
    let mut format: Option<(u16, u16, u32, u16)> = None;
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            if size < 16 || body + 16 > bytes.len() {
                return Err(WavError::Truncated);
            }
            let le16 = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
            let mut tag = le16(body);
            let channels = le16(body + 2);
            let rate = u32::from_le_bytes([
                bytes[body + 4],
                bytes[body + 5],
                bytes[body + 6],
                bytes[body + 7],
            ]);
            let bits = le16(body + 14);
            if tag == FORMAT_EXTENSIBLE {
                // cbSize(2) validBits(2) channelMask(4) then the sub-format GUID, whose first two
                // bytes are the plain format tag.
                if size < 26 || body + 26 > bytes.len() {
                    return Err(WavError::Truncated);
                }
                tag = le16(body + 24);
            }
            format = Some((tag, channels, rate, bits));
        } else if id == b"data" {
            let (tag, channels, rate, bits) = format.ok_or(WavError::Unsupported(
                "data chunk before the fmt chunk".to_string(),
            ))?;
            validate_format(channels, rate)?;
            let end = body.saturating_add(size).min(bytes.len());
            let samples = convert_samples(&bytes[body..end], tag, bits)?;
            return Ok(DecodedWav {
                sample_rate: rate,
                channels,
                samples,
            });
        }
        // Chunks are padded to an even size.
        pos = body.saturating_add(size).saturating_add(size & 1);
    }
    Err(WavError::Truncated)
}

fn validate_format(channels: u16, rate: u32) -> Result<(), WavError> {
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(WavError::Unsupported(format!("{channels} channels")));
    }
    if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&rate) {
        return Err(WavError::Unsupported(format!("{rate} Hz")));
    }
    Ok(())
}

fn convert_samples(data: &[u8], tag: u16, bits: u16) -> Result<Vec<i16>, WavError> {
    match (tag, bits) {
        (FORMAT_PCM, 16) => Ok(data
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect()),
        (FORMAT_PCM, 24) => Ok(data
            .chunks_exact(3)
            .map(|b| i16::from_le_bytes([b[1], b[2]]))
            .collect()),
        (FORMAT_PCM, 32) => Ok(data
            .chunks_exact(4)
            .map(|b| i16::from_le_bytes([b[2], b[3]]))
            .collect()),
        (FORMAT_FLOAT, 32) => Ok(data
            .chunks_exact(4)
            .map(|b| float_to_i16(f32::from_le_bytes([b[0], b[1], b[2], b[3]])))
            .collect()),
        _ => Err(WavError::Unsupported(format!(
            "format {tag} with {bits} bits"
        ))),
    }
}

fn float_to_i16(value: f32) -> i16 {
    if value.is_nan() {
        return 0;
    }
    (value.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

/// Averages interleaved channels into one.
pub(crate) fn downmix_to_mono(samples: &[i16], channels: u16) -> Vec<i16> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let channels = usize::from(channels);
    samples
        .chunks_exact(channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|&s| i32::from(s)).sum();
            (sum / channels as i32) as i16
        })
        .collect()
}

/// Linear-interpolation resampling of mono samples; good enough for speech.
pub(crate) fn resample_linear(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || samples.is_empty() || from_rate == 0 || to_rate == 0 {
        return samples.to_vec();
    }
    let out_len = ((samples.len() as u64 * u64::from(to_rate)) / u64::from(from_rate)) as usize;
    let step = f64::from(from_rate) / f64::from(to_rate);
    let last = samples.len() - 1;
    (0..out_len)
        .map(|i| {
            let position = i as f64 * step;
            let index = (position.floor() as usize).min(last);
            let next = (index + 1).min(last);
            let fraction = position - index as f64;
            let value =
                f64::from(samples[index]) * (1.0 - fraction) + f64::from(samples[next]) * fraction;
            value.round() as i16
        })
        .collect()
}

/// Little-endian 16-bit PCM from base64; an odd trailing byte is an error.
pub(crate) fn pcm16_from_base64(data: &str) -> Result<Vec<i16>, String> {
    let bytes = BASE64_STANDARD
        .decode(data)
        .map_err(|_| "audio data is not valid base64".to_string())?;
    if bytes.len() % 2 != 0 {
        return Err("audio data is not 16-bit PCM (odd byte count)".to_string());
    }
    Ok(bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect())
}

pub(crate) fn pcm16_to_base64(samples: &[i16]) -> String {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    BASE64_STANDARD.encode(bytes)
}

/// A synthesized WAV as mono samples at [`OUTPUT_SAMPLE_RATE`].
pub(crate) fn to_output_pcm(wav: &[u8]) -> Result<Vec<i16>, WavError> {
    let decoded = decode(wav)?;
    let mono = downmix_to_mono(&decoded.samples, decoded.channels);
    Ok(resample_linear(
        &mono,
        decoded.sample_rate,
        OUTPUT_SAMPLE_RATE,
    ))
}

#[cfg(test)]
#[path = "wav_tests.rs"]
mod tests;
