//! Heuristic default output format for a path or URL.

use super::{OutputFormat, is_audio_output, is_image_output, is_subtitle_output, is_video_output};
use std::path::Path;

/// Suggest a default [`OutputFormat`] for a local path based on its extension.
///
/// Rules:
/// - video containers → MP4
/// - audio → MP3
/// - still images → PNG
/// - PDF / office / HTML → Markdown
/// - default → Markdown
pub fn suggested_output_for_path(path: impl AsRef<Path>) -> OutputFormat {
    let Some(ext) = path
        .as_ref()
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
    else {
        return OutputFormat::MARKDOWN;
    };

    // Treat the extension as if it were an output format id when possible.
    if let Ok(as_format) = ext.parse::<OutputFormat>() {
        if is_video_output(as_format)
            || matches!(
                ext.as_str(),
                "m2ts" | "mts" | "vob" | "wmv" | "flv" | "rm" | "rmvb" | "asf" | "divx"
            )
        {
            return OutputFormat::MP4;
        }
        if is_audio_output(as_format)
            || matches!(
                ext.as_str(),
                "amr" | "ape" | "dts" | "eac3" | "mpc" | "oga" | "spx"
            )
        {
            return OutputFormat::MP3;
        }
        if is_image_output(as_format)
            || matches!(
                ext.as_str(),
                "bmp" | "tif" | "tiff" | "webp" | "heic" | "jpeg"
            )
        {
            return OutputFormat::PNG;
        }
        if is_subtitle_output(as_format) {
            return OutputFormat::SRT;
        }
    }

    match ext.as_str() {
        // Video demux extensions without matching OutputFormat ids.
        "m2ts" | "mts" | "vob" | "wmv" | "flv" | "rm" | "rmvb" | "asf" | "divx" | "mxf" => {
            OutputFormat::MP4
        }
        // Audio
        "amr" | "ape" | "dts" | "eac3" | "mpc" | "oga" | "spx" => OutputFormat::MP3,
        // Stills
        "bmp" | "tif" | "tiff" | "webp" | "heic" | "jpeg" => OutputFormat::PNG,
        // Documents / web → Markdown
        "pdf" | "docx" | "pptx" | "xlsx" | "xls" | "odt" | "ods" | "odp" | "epub" | "html"
        | "htm" | "xhtml" | "rtf" | "md" | "markdown" | "txt" | "csv" | "json" | "xml" => {
            OutputFormat::MARKDOWN
        }
        _ => OutputFormat::MARKDOWN,
    }
}

/// URL conversions always default to clean Markdown article extraction.
pub fn suggested_output_for_url() -> OutputFormat {
    OutputFormat::MARKDOWN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_media_and_document_defaults() {
        assert_eq!(suggested_output_for_path("clip.mp4"), OutputFormat::MP4);
        assert_eq!(suggested_output_for_path("clip.MOV"), OutputFormat::MP4);
        assert_eq!(suggested_output_for_path("track.wav"), OutputFormat::MP3);
        assert_eq!(suggested_output_for_path("photo.jpg"), OutputFormat::PNG);
        assert_eq!(
            suggested_output_for_path("scan.pdf"),
            OutputFormat::MARKDOWN
        );
        assert_eq!(
            suggested_output_for_path("report.docx"),
            OutputFormat::MARKDOWN
        );
        assert_eq!(
            suggested_output_for_path("page.html"),
            OutputFormat::MARKDOWN
        );
        assert_eq!(
            suggested_output_for_path("mystery.bin"),
            OutputFormat::MARKDOWN
        );
        assert_eq!(suggested_output_for_url(), OutputFormat::MARKDOWN);
    }

    #[test]
    fn suggests_edge_case_extensions() {
        // Video demux and media container special cases.
        assert_eq!(suggested_output_for_path("tape.mxf"), OutputFormat::MP4);
        // Subtitle inputs re-export as SRT.
        assert_eq!(suggested_output_for_path("subs.srt"), OutputFormat::SRT);
        assert_eq!(suggested_output_for_path("subs.vtt"), OutputFormat::SRT);
        // Uppercase and compound extensions still classify correctly.
        assert_eq!(suggested_output_for_path("Photo.JPEG"), OutputFormat::PNG);
        assert_eq!(
            suggested_output_for_path("archive.tar.gz"),
            OutputFormat::MARKDOWN
        );
        // No extension or unknown extension falls back to Markdown.
        assert_eq!(suggested_output_for_path("README"), OutputFormat::MARKDOWN);
        assert_eq!(
            suggested_output_for_path("data.unknown"),
            OutputFormat::MARKDOWN
        );
    }
}
