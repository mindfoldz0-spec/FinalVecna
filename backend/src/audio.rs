/// WAV validation + acoustic features for the 2-way audio pipeline.
///
/// Accepts 16-bit PCM mono WAV (what the browser sends). Rejects everything
/// else with a reason string — never silently scores garbage.
pub fn validate_wav(bytes: &[u8]) -> Result<WavInfo, &'static str> {
    if bytes.len() < 44 {
        return Err("too short for WAV header");
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not RIFF/WAVE");
    }
    let mut offset = 12;
    let mut fmt: Option<(u16, u16, u32)> = None; // (format, channels, sample_rate)
    let mut bits: Option<u16> = None;
    let mut data_len: Option<usize> = None;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size =
            u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let payload = offset + 8;
        if payload + size > bytes.len() {
            return Err("chunk overruns buffer");
        }
        if id == b"fmt " {
            if size < 16 {
                return Err("bad fmt chunk");
            }
            let format = u16::from_le_bytes(bytes[payload..payload + 2].try_into().unwrap());
            let channels = u16::from_le_bytes(bytes[payload + 2..payload + 4].try_into().unwrap());
            let rate = u32::from_le_bytes(bytes[payload + 4..payload + 8].try_into().unwrap());
            let b = u16::from_le_bytes(bytes[payload + 14..payload + 16].try_into().unwrap());
            fmt = Some((format, channels, rate));
            bits = Some(b);
        } else if id == b"data" {
            data_len = Some(size);
        }
        offset = payload + size + (size % 2);
    }
    let (format, channels, rate) = fmt.ok_or("missing fmt chunk")?;
    let b = bits.ok_or("missing bits")?;
    let data_len = data_len.ok_or("missing data chunk")?;
    if format != 1 {
        return Err("only PCM WAV accepted");
    }
    if channels != 1 {
        return Err("only mono WAV accepted");
    }
    if b != 16 {
        return Err("only 16-bit WAV accepted");
    }
    Ok(WavInfo {
        sample_rate: rate,
        channels,
        bits: b,
        data_len,
    })
}

#[derive(Debug)]
pub struct WavInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub data_len: usize,
}

#[derive(Debug, Default)]
pub struct AcousticFeatures {
    pub rms: f32,
    pub zcr: f32,
    pub loudness_factor: f32,
}

/// RMS + zero-crossing over the PCM body (assumes 16-bit mono, validated first).
pub fn acoustic_features(bytes: &[u8]) -> AcousticFeatures {
    let Some(start) = find_data(bytes) else {
        return AcousticFeatures::default();
    };
    let pcm = &bytes[start..];
    let n = pcm.len() / 2;
    if n == 0 {
        return AcousticFeatures::default();
    }
    let mut sum_sq: f64 = 0.0;
    let mut zc: usize = 0;
    let mut prev: i16 = 0;
    for (i, chunk) in pcm.chunks_exact(2).enumerate() {
        let s = i16::from_le_bytes([chunk[0], chunk[1]]);
        sum_sq += (s as f64) * (s as f64);
        if i > 0 && ((s >= 0) != (prev >= 0)) {
            zc += 1;
        }
        prev = s;
    }
    let rms = ((sum_sq / n as f64).sqrt() / 32768.0) as f32;
    let zcr = zc as f32 / n as f32;
    let loudness = (rms / 0.06).clamp(0.5, 2.0);
    AcousticFeatures { rms, zcr, loudness_factor: loudness }
}

fn find_data(bytes: &[u8]) -> Option<usize> {
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let size =
            u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().ok()?) as usize;
        if &bytes[offset..offset + 4] == b"data" {
            return Some(offset + 8);
        }
        offset += 8 + size + (size % 2);
    }
    None
}
