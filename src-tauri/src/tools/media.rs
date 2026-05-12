use crate::provider::ToolDefinition;
use serde_json::json;
use std::path::Path;
use std::process::Command;

pub fn vision_analyze_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "vision_analyze".into(),
            description: "Analyzes an image file. Returns dimensions, format, color profile, and metadata. For AI-based visual analysis (object detection, text OCR), use with a vision-capable model.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "imagePath": {
                        "type": "string",
                        "description": "Path to the image file (png, jpg, gif, bmp, heic, webp, tiff)"
                    }
                },
                "required": ["imagePath"]
            }),
        },
    }
}

pub fn text_to_speech_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "text_to_speech".into(),
            description: "Converts text to speech using the configured TTS provider. Supports macOS say (default), OpenAI TTS, and Microsoft Edge TTS (free).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to speak. Leave empty to list current TTS configuration."
                    },
                    "voice": {
                        "type": "string",
                        "description": "Voice name. macOS: Samantha, Alex, etc. OpenAI: alloy, echo, fable, nova, onyx, shimmer. Edge: en-US-AriaNeural, tr-TR-EmelNeural, etc."
                    },
                    "outputFile": {
                        "type": "string",
                        "description": "Optional path to save audio file (.aiff, .m4a or .mp3)"
                    },
                    "rate": {
                        "type": "integer",
                        "description": "Speaking rate in words per minute (macOS only, default 175)"
                    }
                },
                "required": []
            }),
        },
    }
}

pub async fn handle_vision_analyze(args: serde_json::Value) -> Result<String, String> {
    let image_path = args["imagePath"].as_str().ok_or("imagePath required")?;
    let path = Path::new(image_path);
    if !path.exists() {
        return Err(format!("File not found: {}", image_path));
    }

    let ext = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("unknown")
        .to_lowercase();

    let is_image = matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "heic" | "webp" | "tiff" | "tif" | "ico" | "avif"
    );

    if !is_image {
        return Err(format!("Not a recognized image format: .{}", ext));
    }

    let file_size = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(0);

    let mut lines = Vec::new();
    lines.push(format!("File: {}", image_path));
    lines.push(format!("Format: {}{}", ext.to_uppercase(),
        if ext == "jpeg" { " (JFIF)" } else { "" }));
    lines.push(format!("Size: {} bytes ({:.1} KB)", file_size, file_size as f64 / 1024.0));

    #[cfg(target_os = "macos")]
    {
        let output = Command::new("sips")
            .args(["-g", "all", image_path])
            .output()
            .ok();

        if let Some(out) = output {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                for raw_line in text.lines() {
                    let line = raw_line.trim();
                    if line.is_empty() || line.starts_with("/") || line.starts_with("  ") && !line.contains(":") {
                        continue;
                    }
                    let clean = line.trim_start();
                    if clean.contains(":") {
                        let parts: Vec<&str> = clean.splitn(2, ':').collect();
                        let key = parts[0].trim();
                        let val = parts.get(1).map(|v| v.trim()).unwrap_or("");
                        if !val.is_empty() {
                            lines.push(format!("  {}: {}", key, val));
                        }
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        lines.push("  (metadata extraction via sips is macOS-only)");
    }

    Ok(lines.join("\n"))
}

pub async fn handle_text_to_speech(
    args: serde_json::Value,
    provider: &str,
    api_key: Option<&str>,
    base_url: &str,
    model: &str,
    voice_default: &str,
) -> Result<String, String> {
    let text = args["text"].as_str();
    let voice = args["voice"].as_str().unwrap_or(voice_default);
    let output_file = args["outputFile"].as_str();
    let _rate = args["rate"].as_u64();

    if text.is_none() && voice != "list" {
        let provider_info = match provider {
            "edge" => format!("Provider: Microsoft Edge TTS (free, no API key required)\nDefault voice: {}", voice_default),
            "openai" => format!("Provider: OpenAI TTS\nModel: {}\nDefault voice: {}", model, voice_default),
            _ => "Provider: macOS say (built-in)".to_string(),
        };
        return Ok(format!(
            "TTS Configuration:\n{}\n\nUse 'list' as voice to see available voices.",
            provider_info
        ));
    }

    match provider {
        "edge" => handle_edge_tts(text, voice, output_file).await,
        "openai" => handle_openai_tts(text, voice, output_file, api_key, base_url, model).await,
        _ => handle_macos_tts(text, voice, output_file, _rate).await,
    }
}

async fn handle_macos_tts(
    text: Option<&str>,
    voice: &str,
    output_file: Option<&str>,
    _rate: Option<u64>,
) -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        if voice == "list" || text.is_none() {
            let output = Command::new("say")
                .args(["-v", "?"])
                .output()
                .map_err(|e| format!("say command failed: {}", e))?;
            let voices = String::from_utf8_lossy(&output.stdout);
            let voice_list: Vec<&str> = voices.lines()
                .filter(|l| !l.is_empty())
                .take(50)
                .collect();
            return Ok(format!("Available macOS voices (first 50):\n{}", voice_list.join("\n")));
        }

        let text = text.ok_or("text is required for speech")?;
        let mut cmd = Command::new("say");

        if voice != "default" && !voice.is_empty() {
            cmd.args(["-v", voice]);
        }
        if let Some(f) = output_file {
            cmd.args(["-o", f]);
        }
        if let Some(r) = _rate {
            cmd.args(["-r", &r.to_string()]);
        }
        cmd.arg(text);

        let output = cmd.output().map_err(|e| format!("say command failed: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("say failed: {}", stderr.trim()));
        }

        if let Some(f) = output_file {
            Ok(format!("Audio saved to: {}", f))
        } else {
            Ok("Spoken via macOS TTS".to_string())
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err("macOS TTS only available on macOS. Configure 'edge' or 'openai' provider in [tts] config.".to_string())
    }
}

async fn handle_edge_tts(
    text: Option<&str>,
    voice: &str,
    output_file: Option<&str>,
) -> Result<String, String> {
    let text = text.ok_or("text is required for speech")?;
    let voice_name = if voice == "default" || voice.is_empty() {
        "en-US-AriaNeural"
    } else {
        voice
    };

    let ssml = format!(
        r#"<speak version='1.0' xml:lang='en-US'><voice xml:lang='en-US' name='{}'>{}</voice></speak>"#,
        voice_name, text
    );

    let client = reqwest::Client::new();
    let resp = client
        .post("https://speech.platform.bing.com/consumer/speech/synthesize/readaloud?format=audio-16khz-32kbitrate-mono-mp3")
        .header("Content-Type", "application/ssml+xml")
        .header("User-Agent", "Mozilla/5.0")
        .header("X-Microsoft-OutputFormat", "audio-16khz-32kbitrate-mono-mp3")
        .body(ssml)
        .send()
        .await
        .map_err(|e| format!("Edge TTS request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Edge TTS failed with status: {}", resp.status()));
    }

    let audio = resp.bytes().await
        .map_err(|e| format!("Failed to read Edge TTS response: {}", e))?;

    if audio.is_empty() {
        return Err("Edge TTS returned empty audio".to_string());
    }

    if let Some(f) = output_file {
        std::fs::write(f, &audio)
            .map_err(|e| format!("Failed to write audio: {}", e))?;
        Ok(format!("Audio saved to: {}", f))
    } else {
        let tmp = std::env::temp_dir().join(format!("goblin_tts_{}.mp3", std::process::id()));
        std::fs::write(&tmp, &audio)
            .map_err(|e| format!("Failed to write temp audio: {}", e))?;

        let tmp_path = tmp.to_str().unwrap().to_string();

        #[cfg(target_os = "macos")]
        {
            Command::new("afplay")
                .arg(&tmp_path)
                .spawn()
                .map_err(|e| format!("afplay failed: {}", e))?;
        }

        #[cfg(target_os = "windows")]
        {
            Command::new("powershell")
                .args(["-c", &format!("(New-Object Media.SoundPlayer '{}').PlaySync()", tmp_path)])
                .spawn()
                .ok();
        }

        #[cfg(target_os = "linux")]
        {
            Command::new("paplay")
                .arg(&tmp_path)
                .spawn()
                .ok();
        }

        Ok(format!("Spoken via Edge TTS ({})", voice_name))
    }
}

async fn handle_openai_tts(
    text: Option<&str>,
    voice: &str,
    output_file: Option<&str>,
    api_key: Option<&str>,
    base_url: &str,
    model: &str,
) -> Result<String, String> {
    let text = text.ok_or("text is required for speech")?;
    let api_key = api_key.ok_or("OpenAI TTS requires an API key. Set [tts] provider = \"openai\" with api_key in config.")?;

    let voice_name = if voice == "default" || voice.is_empty() { "alloy" } else { voice };

    let client = reqwest::Client::new();
    let url = format!("{}/audio/speech", base_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "input": text,
        "voice": voice_name,
    });

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("OpenAI TTS request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("OpenAI TTS failed ({}): {}", status, body));
    }

    let audio = resp.bytes().await
        .map_err(|e| format!("Failed to read OpenAI TTS response: {}", e))?;

    if let Some(f) = output_file {
        std::fs::write(f, &audio)
            .map_err(|e| format!("Failed to write audio: {}", e))?;
        Ok(format!("Audio saved to: {}", f))
    } else {
        let tmp = std::env::temp_dir().join(format!("goblin_tts_{}.mp3", std::process::id()));
        std::fs::write(&tmp, &audio)
            .map_err(|e| format!("Failed to write temp audio: {}", e))?;

        let tmp_path = tmp.to_str().unwrap().to_string();

        #[cfg(target_os = "macos")]
        {
            Command::new("afplay")
                .arg(&tmp_path)
                .spawn()
                .map_err(|e| format!("afplay failed: {}", e))?;
        }

        #[cfg(target_os = "windows")]
        {
            Command::new("powershell")
                .args(["-c", &format!("(New-Object Media.SoundPlayer '{}').PlaySync()", tmp_path)])
                .spawn()
                .ok();
        }

        #[cfg(target_os = "linux")]
        {
            Command::new("paplay")
                .arg(&tmp_path)
                .spawn()
                .ok();
        }

        Ok(format!("Spoken via OpenAI TTS ({})", voice_name))
    }
}

pub fn voice_record_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "voice_record".into(),
            description: "Records audio from the default microphone and transcribes it to text. Uses silence detection to auto-stop or records for a fixed duration. Requires ffmpeg (or sox) for recording. Transcription uses the configured STT provider (OpenAI Whisper API by default).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "duration": {
                        "type": "integer",
                        "description": "Maximum recording duration in seconds (default: 10). Recording stops earlier if silence is detected."
                    },
                    "language": {
                        "type": "string",
                        "description": "Language code for transcription (e.g. 'en', 'tr', 'ja'). Leave empty for auto-detect."
                    }
                },
                "required": []
            }),
        },
    }
}

pub async fn handle_voice_record(
    args: serde_json::Value,
    stt_api_key: Option<String>,
    stt_base_url: Option<String>,
) -> Result<String, String> {
    let duration = args["duration"].as_u64().unwrap_or(10);
    let _language = args["language"].as_str();

    let tmp_dir = std::env::temp_dir();
    let audio_path = tmp_dir.join(format!("goblin_record_{}.wav", std::process::id()));

    #[cfg(target_os = "macos")]
    {
        let which_sox = Command::new("which").arg("sox").output().ok()
            .and_then(|o| if o.status.success() { Some(String::from_utf8_lossy(&o.stdout).trim().to_string()) } else { None });

        if which_sox.is_some() {
            let output = Command::new("sox")
                .args([
                    "-d",
                    "-r", "16000",
                    "-c", "1",
                    "-b", "16",
                    audio_path.to_str().unwrap(),
                    "silence", "1", "0.1", "2%",
                    "1", "2.0", "2%",
                ])
                .output()
                .map_err(|e| format!("sox record failed: {}", e))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("sox record failed: {}", stderr.trim()));
            }
        } else {
            let output = Command::new("ffmpeg")
                .args([
                    "-y",
                    "-f", "avfoundation",
                    "-i", ":0",
                    "-t", &duration.to_string(),
                    "-ar", "16000",
                    "-ac", "1",
                    "-sample_fmt", "s16",
                    audio_path.to_str().unwrap(),
                ])
                .output()
                .map_err(|e| format!("ffmpeg record failed: {}", e))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("ffmpeg record failed: {}", stderr.trim()));
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        #[cfg(target_os = "linux")]
        {
            let output = Command::new("arecord")
                .args([
                    "-d", &duration.to_string(),
                    "-r", "16000",
                    "-f", "S16_LE",
                    "-c", "1",
                    audio_path.to_str().unwrap(),
                ])
                .output()
                .map_err(|e| format!("arecord failed: {}", e))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("arecord failed: {}", stderr.trim()));
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            #[cfg(target_os = "windows")]
            {
                // Windows: use ffmpeg if available
                let ffmpeg_check = Command::new("ffmpeg").arg("-version").output().ok();
                if ffmpeg_check.is_some() {
                    let output = Command::new("ffmpeg")
                        .args([
                            "-y",
                            "-f", "dshow",
                            "-i", "audio=default",
                            "-t", &duration.to_string(),
                            "-ar", "16000",
                            "-ac", "1",
                            "-sample_fmt", "s16",
                            audio_path.to_str().unwrap(),
                        ])
                        .output()
                        .map_err(|e| format!("ffmpeg record failed: {}", e))?;

                    if !output.status.success() {
                        return Err("ffmpeg recording failed on Windows".to_string());
                    }
                } else {
                    return Err("voice_record on Windows requires ffmpeg. Install: winget install ffmpeg".to_string());
                }
            }

            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            {
                return Err("voice_record currently requires macOS (ffmpeg/sox), Linux (arecord), or Windows (ffmpeg)".to_string());
            }
        }
    }

    if !audio_path.exists() {
        return Err("Recording failed: no audio file produced".to_string());
    }

    let api_key = match stt_api_key {
        Some(k) if !k.is_empty() => k,
        _ => {
            std::fs::remove_file(&audio_path).ok();
            return Err(
                "STT not configured. Add [stt] section to ~/.goblin/config.toml:\n\
                 [stt]\n\
                 provider = \"openai\"\n\
                 api_key = \"sk-...\"".to_string()
            );
        }
    };

    let base_url = stt_base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let audio_data = std::fs::read(&audio_path)
        .map_err(|e| format!("Failed to read audio: {}", e))?;

    let client = reqwest::Client::new();
    let whisper_url = format!("{}/audio/transcriptions", base_url.trim_end_matches('/'));

    let part = reqwest::multipart::Part::bytes(audio_data)
        .file_name("recording.wav")
        .mime_str("audio/wav")
        .map_err(|e| format!("multipart error: {}", e))?;

    let mut form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);

    if let Some(lang) = _language {
        if !lang.is_empty() {
            form = form.text("language", lang.to_string());
        }
    }

    let resp = client
        .post(&whisper_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("Whisper API request failed: {}", e))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    std::fs::remove_file(&audio_path).ok();

    if !status.is_success() {
        return Err(format!("Whisper API error ({}): {}", status, body));
    }

    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("Failed to parse Whisper response: {}", e))?;

    let text = parsed["text"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    if text.is_empty() {
        Ok("(no speech detected)".to_string())
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defs_exist() {
        let v = vision_analyze_def();
        let t = text_to_speech_def();
        assert_eq!(v.function.name, "vision_analyze");
        assert_eq!(t.function.name, "text_to_speech");
    }

    #[tokio::test]
    async fn test_vision_missing_file() {
        let result = handle_vision_analyze(serde_json::json!({"imagePath": "/nonexistent.png"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_vision_bad_format() {
        let result = handle_vision_analyze(serde_json::json!({"imagePath": "Cargo.toml"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_tts_list_voices() {
        let result = handle_text_to_speech(
            serde_json::json!({"voice": "list"}),
            "macos",
            None,
            "",
            "",
            "",
        ).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_tts_config_info() {
        let result = handle_text_to_speech(
            serde_json::json!({}),
            "edge",
            None,
            "",
            "",
            "en-US-AriaNeural",
        ).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("Edge TTS"));
    }
}
