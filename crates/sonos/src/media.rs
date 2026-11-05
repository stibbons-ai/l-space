//! Which audio files a Group plays, and as what media type.

/// The media type Sonos accepts for an audio file named `file_name`, by its extension; none
/// for a format Sonos doesn't play, which a Group can't be given. Per Sonos's supported audio
/// formats (`docs/research/sonos-media-types.md`).
pub fn media_type(file_name: &str) -> Option<&'static str> {
    let (_, extension) = file_name.rsplit_once('.')?;
    Some(match extension.to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "m4a" | "m4b" | "mp4" => "audio/mp4",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "ogg" | "oga" => "application/ogg",
        "wma" | "asf" => "audio/x-ms-wma",
        "wav" => "audio/wav",
        "aif" | "aiff" => "audio/aiff",
        // Ogg Opus among them: Sonos skips it silently (docs/research/sonos-smoke-2026-11-04.md).
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::media_type;

    #[test]
    fn audio_formats_sonos_plays_have_its_media_types_and_others_none() {
        let cases = [
            ("Episode 1.mp3", Some("audio/mpeg")),
            ("book.M4B", Some("audio/mp4")),
            ("talk.m4a", Some("audio/mp4")),
            ("raw.aac", Some("audio/aac")),
            ("lossless.flac", Some("audio/flac")),
            ("vorbis.ogg", Some("application/ogg")),
            ("silence.wav", Some("audio/wav")),
            ("old.aiff", Some("audio/aiff")),
            ("legacy.wma", Some("audio/x-ms-wma")),
            ("speech.opus", None),
            ("cover.jpg", None),
            ("no-extension", None),
        ];
        for (name, expected) in cases {
            assert_eq!(media_type(name), expected, "{name}");
        }
    }
}
