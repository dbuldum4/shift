//! Heuristic default output format for a path or URL.

use super::{OutputFormat, is_audio_output, is_image_output, is_subtitle_output, is_video_output};
use std::path::Path;

/// Suggest a default [`OutputFormat`] for a local path based on its extension.
///
/// Rules:
/// - video containers → MP4
/// - audio → MP3
/// - still images → PNG
/// - sheet-native tabular files → CSV (spreadsheet module)
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
            || matches!(ext.as_str(), "bmp" | "tif" | "tiff" | "webp" | "jpeg")
        {
            return OutputFormat::PNG;
        }
        if is_subtitle_output(as_format) {
            return OutputFormat::SRT;
        }
        // Tabular writers (csv/tsv/xlsx) suggest CSV so the spreadsheet module
        // wins by default; document → Markdown remains available via the picker.
        if matches!(
            as_format,
            OutputFormat::CSV | OutputFormat::TSV | OutputFormat::XLSX
        ) {
            return OutputFormat::CSV;
        }
    }

    match ext.as_str() {
        // Video demux extensions without matching OutputFormat ids.
        "m2ts" | "mts" | "vob" | "wmv" | "flv" | "rm" | "rmvb" | "asf" | "divx" | "mxf" | "mpg"
        | "mpg2" => OutputFormat::MP4,
        // Audio
        "amr" | "ape" | "dts" | "eac3" | "mpc" | "oga" | "spx" | "aif" | "aiff" => {
            OutputFormat::MP3
        }
        // Stills
        "bmp" | "tif" | "tiff" | "webp" | "jpeg" => OutputFormat::PNG,
        // Sheet-native → CSV (spreadsheet module owns these pairs).
        "xlsx" | "xlsm" | "xlsb" | "xls" | "xla" | "xlam" | "ods" | "csv" | "tsv" => {
            OutputFormat::CSV
        }
        // Documents / web → Markdown
        "pdf" | "docx" | "pptx" | "odt" | "odp" | "epub" | "html" | "htm" | "xhtml" | "rtf"
        | "md" | "markdown" | "txt" | "json" | "xml" => OutputFormat::MARKDOWN,
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

    #[test]
    fn table_driven_video_demux_extensions_suggest_mp4() {
        // Covers both the parse-time demux aliases and the outer match arm.
        let cases = [
            "m2ts", "mts", "vob", "wmv", "flv", "rm", "rmvb", "asf", "divx", "mxf", "mpg", "mpg2",
        ];
        for ext in cases {
            let path = format!("clip.{ext}");
            assert_eq!(
                suggested_output_for_path(&path),
                OutputFormat::MP4,
                "video demux extension .{ext}"
            );
        }
    }

    #[test]
    fn table_driven_audio_special_extensions_suggest_mp3() {
        let cases = [
            "amr", "ape", "dts", "eac3", "mpc", "oga", "spx", "aif", "aiff",
        ];
        for ext in cases {
            let path = format!("track.{ext}");
            assert_eq!(
                suggested_output_for_path(&path),
                OutputFormat::MP3,
                "audio special extension .{ext}"
            );
        }
    }

    #[test]
    fn table_driven_still_extensions_suggest_png() {
        let cases = ["bmp", "tif", "tiff", "webp", "jpeg"];
        for ext in cases {
            let path = format!("photo.{ext}");
            assert_eq!(
                suggested_output_for_path(&path),
                OutputFormat::PNG,
                "still extension .{ext}"
            );
        }
    }

    #[test]
    fn table_driven_document_extensions_suggest_markdown() {
        let cases = [
            "pdf", "docx", "pptx", "odt", "odp", "epub", "html", "htm", "xhtml", "rtf", "md",
            "markdown", "txt", "json", "xml",
        ];
        for ext in cases {
            let path = format!("doc.{ext}");
            assert_eq!(
                suggested_output_for_path(&path),
                OutputFormat::MARKDOWN,
                "document extension .{ext}"
            );
        }
    }

    #[test]
    fn table_driven_sheet_extensions_suggest_csv() {
        let cases = [
            "xlsx", "xlsm", "xlsb", "xls", "xla", "xlam", "ods", "csv", "tsv",
        ];
        for ext in cases {
            let path = format!("sheet.{ext}");
            assert_eq!(
                suggested_output_for_path(&path),
                OutputFormat::CSV,
                "sheet extension .{ext}"
            );
        }
    }

    #[test]
    fn table_driven_known_format_ids() {
        // Video OutputFormat ids demux to MP4.
        for ext in ["mkv", "webm", "mp4", "mov", "avi"] {
            assert_eq!(
                suggested_output_for_path(format!("clip.{ext}")),
                OutputFormat::MP4,
                "known video id .{ext}"
            );
        }
        // GIF is catalogued as a video writer id, so it demuxes to MP4.
        assert_eq!(suggested_output_for_path("anim.gif"), OutputFormat::MP4);

        // Audio OutputFormat ids → MP3.
        for ext in ["flac", "aac", "mp3", "wav", "ogg", "opus", "m4a"] {
            assert_eq!(
                suggested_output_for_path(format!("track.{ext}")),
                OutputFormat::MP3,
                "known audio id .{ext}"
            );
        }

        // Still OutputFormat ids → PNG.
        for ext in ["png", "jpg"] {
            assert_eq!(
                suggested_output_for_path(format!("photo.{ext}")),
                OutputFormat::PNG,
                "known still id .{ext}"
            );
        }

        // Subtitle OutputFormat ids → SRT. Unknown subtitle-like extensions
        // (e.g. ass) fall through to the default Markdown arm.
        for ext in ["srt", "vtt"] {
            assert_eq!(
                suggested_output_for_path(format!("subs.{ext}")),
                OutputFormat::SRT,
                "known subtitle id .{ext}"
            );
        }
        assert_eq!(
            suggested_output_for_path("subs.ass"),
            OutputFormat::MARKDOWN
        );
    }

    #[test]
    fn table_driven_case_insensitive_samples() {
        let cases = [
            ("Tape.MXF", OutputFormat::MP4),
            ("Clip.M2TS", OutputFormat::MP4),
            ("Track.AMR", OutputFormat::MP3),
            ("Sound.AIFF", OutputFormat::MP3),
            ("Photo.JPEG", OutputFormat::PNG),
            ("Image.WEBP", OutputFormat::PNG),
            ("Scan.PDF", OutputFormat::MARKDOWN),
            ("Page.HTML", OutputFormat::MARKDOWN),
            ("Subs.SRT", OutputFormat::SRT),
            ("Clip.MKV", OutputFormat::MP4),
            ("Track.FLAC", OutputFormat::MP3),
            ("Photo.PNG", OutputFormat::PNG),
        ];
        for (path, expected) in cases {
            assert_eq!(
                suggested_output_for_path(path),
                expected,
                "case-insensitive sample {path}"
            );
        }
    }

    #[test]
    fn suggested_output_for_url_is_markdown() {
        assert_eq!(suggested_output_for_url(), OutputFormat::MARKDOWN);
    }

    #[test]
    fn ffmpeg_input_list_extensions_have_suggestions() {
        // Mirror FFmpeg INPUTS families so suggested_output stays complete as demux surface grows.
        let video = [
            // Note: extensions present only on the FFmpeg demux list but without a
            // dedicated suggest arm (e.g. mk3d) fall through to Markdown and are
            // intentionally omitted here.
            "3gp", "asf", "avi", "divx", "flv", "gif", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg",
            "mpg", "mts", "mxf", "rm", "rmvb", "ts", "vob", "webm", "wmv",
        ];
        // Catalog audio ids + the special demux arms in `suggested_output_for_path`.
        // FFmpeg-only demux extensions without a suggest arm (m4b, m4p, mka, weba, …)
        // still fall through to Markdown and are omitted here.
        let audio = [
            "aac", "ac3", "aif", "aiff", "amr", "ape", "caf", "dts", "eac3", "flac", "m4a", "mp3",
            "mpc", "oga", "ogg", "opus", "spx", "wav", "wma",
        ];
        let stills = ["bmp", "jpeg", "jpg", "png", "tif", "tiff", "webp"];

        for ext in video {
            let path = format!("clip.{ext}");
            assert_eq!(
                suggested_output_for_path(&path),
                OutputFormat::MP4,
                "video demux .{ext}"
            );
        }
        for ext in audio {
            let path = format!("track.{ext}");
            assert_eq!(
                suggested_output_for_path(&path),
                OutputFormat::MP3,
                "audio demux .{ext}"
            );
        }
        for ext in stills {
            let path = format!("photo.{ext}");
            assert_eq!(
                suggested_output_for_path(&path),
                OutputFormat::PNG,
                "still demux .{ext}"
            );
        }
    }

    #[test]
    fn remaining_media_catalog_ids_as_inputs() {
        for format in OutputFormat::MEDIA {
            let ext = format.extension();
            // png-sequence-zip is an output-only package; input as .zip is not media demux.
            if format.id() == "png-sequence-zip" {
                continue;
            }
            let path = format!("sample.{ext}");
            let suggested = suggested_output_for_path(&path);
            if is_video_output(*format) {
                assert_eq!(suggested, OutputFormat::MP4, "video id {}", format.id());
            } else if is_audio_output(*format) {
                assert_eq!(suggested, OutputFormat::MP3, "audio id {}", format.id());
            } else if is_image_output(*format) {
                assert_eq!(suggested, OutputFormat::PNG, "image id {}", format.id());
            } else if is_subtitle_output(*format) {
                assert_eq!(suggested, OutputFormat::SRT, "subtitle id {}", format.id());
            }
        }
    }
}
