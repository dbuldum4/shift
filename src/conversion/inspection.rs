//! Bounded, local-only inspection for binary conversion artifacts.
//!
//! This deliberately reads container headers only. It never invokes an external
//! decoder, extracts archive members, or allocates based on a value claimed by
//! an untrusted artifact. That makes it safe to run on the UI's already-loaded
//! result bytes while still giving people useful facts before they save a file.

use super::OutputFormat;

/// Maximum prefix examined by an inspector. This is intentionally independent
/// from the conversion output limit: a large but valid artifact must not make
/// result rendering expensive.
pub const MAX_INSPECTION_PREFIX_BYTES: usize = 1024 * 1024;
/// ZIP's end-of-central-directory record is within the final 65,557 bytes.
pub const MAX_INSPECTION_SUFFIX_BYTES: usize = 70 * 1024;
/// Marker walks (JPEG segments) stop early. The common headers
/// occur at the start; scanning farther would make a malformed artifact slow
/// to render repeatedly in result/history lists.
const MAX_SIGNATURE_SCAN_BYTES: usize = 8 * 1024;

/// Safe facts derived from a binary artifact's container/header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactInspection {
    /// Short, user-facing category such as `Image`, `PDF`, or `Archive`.
    pub kind: &'static str,
    /// The primary identification line.
    pub headline: String,
    /// Bounded detail lines. Values are descriptive only; unknown facts are
    /// omitted rather than guessed.
    pub facts: Vec<String>,
    /// How the preview was produced and the next safe action.
    pub note: String,
}

impl ArtifactInspection {
    pub fn summary(&self) -> String {
        let mut result = self.headline.clone();
        for fact in &self.facts {
            result.push('\n');
            result.push_str(fact);
        }
        result.push_str("\n\n");
        result.push_str(&self.note);
        result
    }
}

/// Inspect a non-text artifact without decoding its payload.
pub fn inspect_binary(format: OutputFormat, bytes: &[u8]) -> ArtifactInspection {
    let prefix = &bytes[..bytes.len().min(MAX_INSPECTION_PREFIX_BYTES)];
    let signature_scan = &prefix[..prefix.len().min(MAX_SIGNATURE_SCAN_BYTES)];
    let suffix_start = bytes.len().saturating_sub(MAX_INSPECTION_SUFFIX_BYTES);
    let suffix = &bytes[suffix_start..];
    let size = format_byte_size(bytes.len() as u64);

    if let Some((name, width, height, extra)) = inspect_image(signature_scan) {
        let mut facts = vec![
            format!("Dimensions: {width} × {height} px"),
            format!("Size: {size}"),
        ];
        if let Some(extra) = extra {
            facts.push(extra);
        }
        return ArtifactInspection {
            kind: "Image",
            headline: format!("Image preview · {name}"),
            facts,
            note: "Header inspection only — use Open for a full-resolution preview.".into(),
        };
    }

    if let Some((version, pages, encrypted)) = inspect_pdf(prefix) {
        let mut facts = vec![format!("Size: {size}")];
        if let Some(pages) = pages {
            facts.push(format!(
                "{pages} page object{} found in the first {}",
                plural(pages),
                scanned_label(prefix.len(), bytes.len())
            ));
        }
        if encrypted {
            facts.push("Encrypted PDF markers detected".into());
        }
        return ArtifactInspection {
            kind: "PDF",
            headline: format!("PDF preview · PDF {version}"),
            facts,
            note: "Header inspection only — Download or Open to view the document.".into(),
        };
    }

    if let Some((name, facts)) = inspect_media(signature_scan, suffix, bytes.len()) {
        let mut facts = facts;
        facts.push(format!("Size: {size}"));
        return ArtifactInspection {
            kind: if is_video_format(format) {
                "Video"
            } else {
                "Audio"
            },
            headline: format!("Media preview · {name}"),
            facts,
            note: "Container headers inspected locally — Open to play with your default app."
                .into(),
        };
    }

    if prefix.starts_with(b"PK")
        && let Some(entries) = inspect_zip(suffix)
    {
        return ArtifactInspection {
            kind: "Archive",
            headline: "Archive preview · ZIP".into(),
            facts: vec![
                format!(
                    "{entries} {}",
                    if entries == 1 { "entry" } else { "entries" }
                ),
                format!("Size: {size}"),
                "Contents were not extracted for preview safety".into(),
            ],
            note: "Archive index inspected locally — Download or Open to browse its contents."
                .into(),
        };
    }

    let kind = if is_image_format(format) {
        "Image"
    } else if format == OutputFormat::PDF {
        "PDF"
    } else if is_video_format(format) {
        "Video"
    } else if is_audio_format(format) {
        "Audio"
    } else if format == OutputFormat::PNG_SEQUENCE_ZIP {
        "Archive"
    } else {
        "Binary"
    };
    ArtifactInspection {
        kind,
        headline: format!("{kind} preview · {}", format.label()),
        facts: vec![
            format!("Size: {size}"),
            format!(
                "Inspected first {}",
                scanned_label(prefix.len(), bytes.len())
            ),
        ],
        note: "The container header was not recognized; Download or Open with your default app."
            .into(),
    }
}

fn inspect_image(bytes: &[u8]) -> Option<(&'static str, u32, u32, Option<String>)> {
    if bytes.len() >= 24 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" && &bytes[12..16] == b"IHDR" {
        let width = be_u32(bytes, 16)?;
        let height = be_u32(bytes, 20)?;
        if width > 0 && height > 0 {
            let color = match bytes.get(25).copied() {
                Some(0) => Some("Grayscale"),
                Some(2) => Some("Truecolor"),
                Some(3) => Some("Indexed color"),
                Some(4) => Some("Grayscale + alpha"),
                Some(6) => Some("Truecolor + alpha"),
                _ => None,
            };
            return Some(("PNG", width, height, color.map(str::to_owned)));
        }
    }
    if bytes.len() >= 10 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        let width = le_u16(bytes, 6)? as u32;
        let height = le_u16(bytes, 8)? as u32;
        if width > 0 && height > 0 {
            return Some(("GIF", width, height, None));
        }
    }
    if bytes.len() >= 26 && &bytes[..2] == b"BM" {
        let width = le_i32(bytes, 18)?;
        let height = le_i32(bytes, 22)?.unsigned_abs();
        if width > 0 && height > 0 {
            return Some(("BMP", width as u32, height, None));
        }
    }
    if bytes.len() >= 30
        && &bytes[..4] == b"RIFF"
        && &bytes[8..12] == b"WEBP"
        && &bytes[12..16] == b"VP8X"
    {
        let width = le_u24(bytes, 24)?.checked_add(1)?;
        let height = le_u24(bytes, 27)?.checked_add(1)?;
        if width > 0 && height > 0 {
            return Some(("WebP", width, height, None));
        }
    }
    inspect_jpeg(bytes)
}

fn inspect_jpeg(bytes: &[u8]) -> Option<(&'static str, u32, u32, Option<String>)> {
    if bytes.len() < 4 || &bytes[..2] != b"\xff\xd8" {
        return None;
    }
    let mut pos = 2;
    while pos + 4 <= bytes.len() {
        if bytes[pos] != 0xff {
            pos += 1;
            continue;
        }
        while pos < bytes.len() && bytes[pos] == 0xff {
            pos += 1;
        }
        let marker = *bytes.get(pos)?;
        pos += 1;
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if (0xd0..=0xd7).contains(&marker) || marker == 0x01 {
            continue;
        }
        let len = be_u16(bytes, pos)? as usize;
        if len < 2 || pos.checked_add(len)? > bytes.len() {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) && len >= 8 {
            let height = be_u16(bytes, pos + 3)? as u32;
            let width = be_u16(bytes, pos + 5)? as u32;
            if width > 0 && height > 0 {
                return Some(("JPEG", width, height, None));
            }
        }
        pos += len;
    }
    None
}

fn inspect_pdf(bytes: &[u8]) -> Option<(String, Option<usize>, bool)> {
    let header = bytes.get(..8)?;
    if !header.starts_with(b"%PDF-") {
        return None;
    }
    let version = std::str::from_utf8(&header[5..])
        .ok()?
        .trim_matches(|c: char| !c.is_ascii_digit() && c != '.')
        .to_owned();
    let pages = count_pdf_page_objects(bytes);
    Some((
        version,
        Some(pages),
        find_bytes(bytes, b"/Encrypt").is_some(),
    ))
}

fn count_pdf_page_objects(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut pos = 0;
    while let Some(found) = find_bytes(&bytes[pos..], b"/Type /Page") {
        let index = pos + found;
        let next = bytes.get(index + b"/Type /Page".len()).copied();
        if !matches!(next, Some(b's')) {
            count += 1;
        }
        pos = index + b"/Type /Page".len();
    }
    count
}

fn inspect_media(
    prefix: &[u8],
    suffix: &[u8],
    total_len: usize,
) -> Option<(&'static str, Vec<String>)> {
    inspect_wav(prefix)
        .or_else(|| inspect_flac(prefix))
        .or_else(|| inspect_mp3(prefix))
        .or_else(|| inspect_ogg(prefix))
        .or_else(|| inspect_mp4(prefix).or_else(|| inspect_mp4(suffix)))
        .or_else(|| inspect_ebml(prefix))
        .or_else(|| inspect_avi(prefix))
        .map(|(name, mut facts)| {
            if prefix.len() < total_len {
                facts.push(format!(
                    "Header scan limited to {}",
                    scanned_label(prefix.len(), total_len)
                ));
            }
            (name, facts)
        })
}

fn inspect_wav(bytes: &[u8]) -> Option<(&'static str, Vec<String>)> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12;
    let mut channels = None;
    let mut sample_rate = None;
    let mut byte_rate = None;
    let mut data_len = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = le_u32(bytes, pos + 4)? as usize;
        let data = pos.checked_add(8)?;
        let end = data.checked_add(size)?;
        if id == b"fmt " && size >= 16 {
            if end > bytes.len() {
                break;
            }
            channels = le_u16(bytes, data + 2);
            sample_rate = le_u32(bytes, data + 4);
            byte_rate = le_u32(bytes, data + 8);
        } else if id == b"data" {
            // The chunk header is enough to report a declared duration. Do not
            // touch its payload, so a malicious length cannot trigger a read.
            data_len = Some(size as u64);
        }
        if end > bytes.len() {
            break;
        }
        pos = end + (size & 1);
    }
    let mut facts = Vec::new();
    if let Some(rate) = sample_rate {
        facts.push(format_hz(rate));
    }
    if let Some(channels) = channels {
        facts.push(channel_label(channels));
    }
    if let (Some(length), Some(rate)) = (data_len, byte_rate) {
        if rate > 0 {
            facts.push(format!(
                "Duration: {}",
                format_duration(length as f64 / rate as f64)
            ));
        }
    }
    Some(("WAV audio", facts))
}

fn inspect_flac(bytes: &[u8]) -> Option<(&'static str, Vec<String>)> {
    if bytes.len() < 42 || &bytes[..4] != b"fLaC" {
        return None;
    }
    // STREAMINFO starts at byte 8 after the 4-byte metadata block header.
    if bytes[4] & 0x7f != 0 || bytes[5..8] != [0, 0, 34] {
        return Some(("FLAC audio", Vec::new()));
    }
    let packed = u64::from_be_bytes(bytes[18..26].try_into().ok()?);
    let sample_rate = ((packed >> 44) & 0xfffff) as u32;
    let channels = ((packed >> 41) & 0x7) as u16 + 1;
    let total_samples = packed & 0x0fff_fffff;
    let mut facts = Vec::new();
    if sample_rate > 0 {
        facts.push(format_hz(sample_rate));
        facts.push(channel_label(channels));
    }
    if sample_rate > 0 && total_samples > 0 {
        facts.push(format!(
            "Duration: {}",
            format_duration(total_samples as f64 / sample_rate as f64)
        ));
    }
    Some(("FLAC audio", facts))
}

fn inspect_mp3(bytes: &[u8]) -> Option<(&'static str, Vec<String>)> {
    let pos = if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
        let size = ((bytes[6] as usize & 0x7f) << 21)
            | ((bytes[7] as usize & 0x7f) << 14)
            | ((bytes[8] as usize & 0x7f) << 7)
            | (bytes[9] as usize & 0x7f);
        10usize.checked_add(size).unwrap_or(bytes.len())
    } else {
        0
    };
    let header = be_u32(bytes, pos)?;
    if header >> 21 != 0x7ff {
        return None;
    }
    let version = (header >> 19) & 0x3;
    let layer = (header >> 17) & 0x3;
    let bitrate_index = ((header >> 12) & 0xf) as usize;
    let sample_index = ((header >> 10) & 0x3) as usize;
    if version == 1 || layer == 0 || bitrate_index == 0 || bitrate_index == 15 || sample_index == 3
    {
        return None;
    }
    let sample_rate = match (version, sample_index) {
        (3, i) => [44_100, 48_000, 32_000][i],
        (2, i) => [22_050, 24_000, 16_000][i],
        (_, i) => [11_025, 12_000, 8_000][i],
    };
    let bitrate = mp3_bitrate_kbps(version, layer, bitrate_index)?;
    let channels = if (header >> 6) & 0x3 == 3 {
        "Mono"
    } else {
        "Stereo"
    };
    Some((
        "MP3 audio",
        vec![
            format!("{bitrate} kbps"),
            format_hz(sample_rate),
            channels.into(),
        ],
    ))
}

fn mp3_bitrate_kbps(version: u32, layer: u32, index: usize) -> Option<u16> {
    let table: &[u16; 16] = match (version == 3, layer) {
        (true, 3) => &[
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
        ],
        (true, 2) => &[
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
        ],
        (true, _) => &[
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
        ],
        (_, 3) => &[
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
        ],
        _ => &[
            0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
        ],
    };
    table.get(index).copied().filter(|value| *value > 0)
}

fn inspect_ogg(bytes: &[u8]) -> Option<(&'static str, Vec<String>)> {
    if !bytes.starts_with(b"OggS") {
        return None;
    }
    let name = if find_bytes(bytes, b"OpusHead").is_some() {
        "Opus audio"
    } else {
        "Ogg media"
    };
    Some((name, Vec::new()))
}

fn inspect_mp4(bytes: &[u8]) -> Option<(&'static str, Vec<String>)> {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return None;
    }
    let brand = std::str::from_utf8(&bytes[8..12]).ok().unwrap_or("MP4");
    let mut facts = vec![format!("Brand: {brand}")];
    if let Some(duration) = find_mp4_duration(bytes) {
        facts.push(format!("Duration: {}", format_duration(duration)));
    }
    Some(("MP4 container", facts))
}

fn find_mp4_duration(bytes: &[u8]) -> Option<f64> {
    let pos = find_bytes(bytes, b"mvhd")?;
    let version = *bytes.get(pos + 4)?;
    let (timescale_at, duration_at, duration) = if version == 1 {
        (
            pos.checked_add(28)?,
            pos.checked_add(32)?,
            u64::from_be_bytes(bytes.get(pos + 32..pos + 40)?.try_into().ok()?),
        )
    } else {
        (
            pos.checked_add(16)?,
            pos.checked_add(20)?,
            be_u32(bytes, pos + 20)? as u64,
        )
    };
    let timescale = be_u32(bytes, timescale_at)? as f64;
    let _ = duration_at;
    (timescale > 0.0).then_some(duration as f64 / timescale)
}

fn inspect_ebml(bytes: &[u8]) -> Option<(&'static str, Vec<String>)> {
    bytes
        .starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
        .then_some(("Matroska/WebM container", Vec::new()))
}

fn inspect_avi(bytes: &[u8]) -> Option<(&'static str, Vec<String>)> {
    (bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"AVI ")
        .then_some(("AVI container", Vec::new()))
}

fn inspect_zip(suffix: &[u8]) -> Option<u16> {
    let sig = b"PK\x05\x06";
    let pos = suffix
        .windows(sig.len())
        .rposition(|window| window == sig)?;
    let record = suffix.get(pos..pos + 22)?;
    le_u16(record, 10)
}

fn find_bytes(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty()).then_some(())?;
    bytes
        .windows(needle.len())
        .position(|window| window == needle)
}

fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn le_i32(bytes: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn le_u24(bytes: &[u8], at: usize) -> Option<u32> {
    let bytes = bytes.get(at..at.checked_add(3)?)?;
    Some(u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16))
}

fn is_image_format(format: OutputFormat) -> bool {
    matches!(
        format.id(),
        "png" | "jpg" | "webp" | "gif" | "tiff" | "bmp" | "heic" | "avif" | "jp2" | "icns"
    )
}
fn is_audio_format(format: OutputFormat) -> bool {
    matches!(
        format.id(),
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "opus" | "ac3" | "wma" | "caf" | "aiff"
    )
}
fn is_video_format(format: OutputFormat) -> bool {
    matches!(
        format.id(),
        "mp4" | "webm" | "mkv" | "mov" | "avi" | "m4v" | "mpeg" | "ts" | "3gp"
    )
}
fn plural(value: usize) -> &'static str {
    if value == 1 { "" } else { "s" }
}
fn scanned_label(scanned: usize, total: usize) -> String {
    if scanned >= total {
        format_byte_size(scanned as u64)
    } else {
        format!(
            "{} of {}",
            format_byte_size(scanned as u64),
            format_byte_size(total as u64)
        )
    }
}
fn format_hz(rate: u32) -> String {
    if rate % 1000 == 0 {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f64 / 1000.0)
    }
}
fn channel_label(channels: u16) -> String {
    match channels {
        1 => "Mono".into(),
        2 => "Stereo".into(),
        n => format!("{n} channels"),
    }
}
fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "unknown".into();
    }
    let rounded = seconds.round() as u64;
    format!("{}:{:02}", rounded / 60, rounded % 60)
}
fn format_byte_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if bytes as f64 >= MIB {
        format!("{:.1} MB", bytes as f64 / MIB)
    } else if bytes as f64 >= KIB {
        format!("{:.1} KB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_image_dimensions_without_decoding() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&800u32.to_be_bytes());
        png.extend_from_slice(&600u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        let preview = inspect_binary(OutputFormat::PNG, &png);
        assert_eq!(preview.kind, "Image");
        assert!(preview.facts.iter().any(|fact| fact.contains("800 × 600")));
    }

    #[test]
    fn pdf_inspection_counts_page_markers_but_not_pages_root() {
        let pdf = b"%PDF-1.7\n1 0 obj << /Type /Pages >>\n2 0 obj << /Type /Page >>\n3 0 obj << /Type /Page >>\n";
        let preview = inspect_binary(OutputFormat::PDF, pdf);
        assert_eq!(preview.kind, "PDF");
        assert!(
            preview
                .facts
                .iter()
                .any(|fact| fact.contains("2 page objects")),
            "{:?}",
            preview.facts
        );
    }

    #[test]
    fn wav_inspection_reports_duration_from_headers_only() {
        let mut wav = b"RIFF\x24\0\0\0WAVEfmt \x10\0\0\0\x01\0\x02\0\x44\xac\0\0\x10\xb1\x02\0\x04\0\x10\0data\xe0\x86\xa1\0".to_vec();
        wav.resize(wav.len() + 1, 0);
        let preview = inspect_binary(OutputFormat::WAV, &wav);
        assert_eq!(preview.kind, "Audio");
        assert!(preview.facts.iter().any(|fact| fact.contains("44.1 kHz")));
        assert!(
            preview
                .facts
                .iter()
                .any(|fact| fact.contains("Duration: 1:00"))
        );
    }

    #[test]
    fn zip_uses_only_eocd_and_never_extracts_entries() {
        let mut zip = vec![0u8; 80_000];
        zip[..4].copy_from_slice(b"PK\x03\x04");
        let at = zip.len() - 22;
        zip[at..at + 4].copy_from_slice(b"PK\x05\x06");
        zip[at + 10..at + 12].copy_from_slice(&3u16.to_le_bytes());
        let preview = inspect_binary(OutputFormat::PNG_SEQUENCE_ZIP, &zip);
        assert_eq!(preview.kind, "Archive");
        assert!(preview.facts.iter().any(|fact| fact.contains("3 entries")));
    }

    #[test]
    fn malformed_and_huge_inputs_stay_bounded() {
        let bytes = vec![0xff; MAX_INSPECTION_PREFIX_BYTES * 2];
        let preview = inspect_binary(OutputFormat::MP4, &bytes);
        assert_eq!(preview.kind, "Video");
        assert!(preview.summary().contains("Inspected first"));
    }
}
