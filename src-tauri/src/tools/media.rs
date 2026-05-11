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
            description: "Converts text to speech using macOS text-to-speech engine. Reads the text aloud or saves to an audio file. Lists available voices when no text provided.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to speak. Leave empty to list available voices."
                    },
                    "voice": {
                        "type": "string",
                        "description": "Voice name (e.g. 'Samantha', 'Alex', 'Daniel'). Use 'list' to see available voices."
                    },
                    "outputFile": {
                        "type": "string",
                        "description": "Optional path to save audio file (.aiff or .m4a). If not provided, speaks directly."
                    },
                    "rate": {
                        "type": "integer",
                        "description": "Speaking rate in words per minute (default: system default, typically 175)"
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

pub async fn handle_text_to_speech(args: serde_json::Value) -> Result<String, String> {
    let text = args["text"].as_str();
    let voice = args["voice"].as_str();
    let output_file = args["outputFile"].as_str();
    let _rate = args["rate"].as_u64();

    #[cfg(not(target_os = "macos"))]
    {
        return Err("text_to_speech is currently macOS-only (uses 'say' command)".to_string());
    }

    #[cfg(target_os = "macos")]
    {
        if voice == Some("list") || text.is_none() {
            let output = Command::new("say")
                .args(["-v", "?"])
                .output()
                .map_err(|e| format!("say command failed: {}", e))?;
            let voices = String::from_utf8_lossy(&output.stdout);
            let voice_list: Vec<&str> = voices.lines()
                .filter(|l| !l.is_empty())
                .take(50)
                .collect();
            return Ok(format!("Available voices (first 50):\n{}", voice_list.join("\n")));
        }

        let text = text.ok_or("text is required for speech")?;
        let mut cmd = Command::new("say");

        if let Some(v) = voice {
            cmd.args(["-v", v]);
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
            Ok("Spoken".to_string())
        }
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
        let result = handle_text_to_speech(serde_json::json!({"voice": "list"})).await;
        assert!(result.is_ok());
    }
}
