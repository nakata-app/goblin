use crate::provider::ToolDefinition;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

fn skills_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let path = Path::new(&home).join(".agents").join("skills");
    path
}

pub fn skill_list_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "skill_list".into(),
            description: "Lists all available agent skills. Checks both project-level (.agents/skills/) and user-level (~/.agents/skills/) directories. Returns skill names with descriptions and locations.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "projectOnly": {
                        "type": "boolean",
                        "description": "Only show project-level skills (default: show both)"
                    }
                },
                "required": []
            }),
        },
    }
}

pub fn skill_view_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "skill_view".into(),
            description: "Reads and displays the full contents of a specific skill's SKILL.md file. Returns the skill's instructions, workflows, and configuration.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "skillName": {
                        "type": "string",
                        "description": "Name of the skill to view (e.g. 'react-doctor', 'premortem', 'agent-reach')"
                    }
                },
                "required": ["skillName"]
            }),
        },
    }
}

pub fn skill_manage_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "skill_manage".into(),
            description: "Manages skills: install from URL/repo, create new skill scaffold, or enable/disable. Use action='install' to add, 'create' to scaffold a new skill, 'remove' to delete.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["install", "create", "remove"],
                        "description": "Action to perform: install (from URL/repo), create (scaffold new skill), remove (delete skill)"
                    },
                    "skillName": {
                        "type": "string",
                        "description": "Skill name (required for create, remove)"
                    },
                    "source": {
                        "type": "string",
                        "description": "Source URL or git repo for install action"
                    },
                    "description": {
                        "type": "string",
                        "description": "Skill description for create action"
                    }
                },
                "required": ["action"]
            }),
        },
    }
}

fn find_skill_dirs() -> Vec<(String, PathBuf)> {
    let mut dirs = Vec::new();

    // Project-level skills
    let project_skills = Path::new(".agents").join("skills");
    if project_skills.is_dir() {
        dirs.push(("project".to_string(), project_skills));
    }

    // User-level skills
    let user_skills = skills_dir();
    if user_skills.is_dir() {
        dirs.push(("user".to_string(), user_skills));
    }

    dirs
}

fn list_skills_in(dir: &Path) -> Vec<(String, String, String)> {
    let mut skills = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skill_md = path.join("SKILL.md");
                if skill_md.exists() {
                    let name = path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let desc = extract_description(&skill_md);
                    skills.push((name, desc, path.to_string_lossy().to_string()));
                }
            }
        }
    }
    skills
}

fn extract_description(skill_md: &Path) -> String {
    match fs::read_to_string(skill_md) {
        Ok(content) => {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("description:") {
                    return trimmed["description:".len()..].trim().to_string();
                }
                if trimmed.starts_with("## ") {
                    return trimmed["## ".len()..].trim().to_string();
                }
            }
            "No description".to_string()
        }
        Err(_) => "Cannot read SKILL.md".to_string(),
    }
}

pub async fn handle_skill_list(args: serde_json::Value) -> Result<String, String> {
    let project_only = args["projectOnly"].as_bool().unwrap_or(false);
    let skill_dirs = find_skill_dirs();

    let mut output = String::from("## Agent Skills\n\n");

    for (scope, dir) in &skill_dirs {
        if project_only && scope == "user" {
            continue;
        }
        let skills = list_skills_in(dir);
        if skills.is_empty() {
            continue;
        }
        output.push_str(&format!("### {} ({})\n", match scope.as_str() {
            "project" => "Project Skills",
            "user" => "User Skills",
            _ => "Skills",
        }, dir.display()));

        for (name, desc, _path) in &skills {
            output.push_str(&format!("- **{}** — {}\n", name, desc));
        }
        output.push('\n');
    }

    if output == "## Agent Skills\n\n" {
        output.push_str("No skills found. Create ~/.agents/skills/ directory or install skills using skill_manage.\n");
    }

    let all_count: usize = skill_dirs.iter().map(|(_, d)| list_skills_in(d).len()).sum();
    output.push_str(&format!("Total: {} skill(s)\n", all_count));

    Ok(output)
}

pub async fn handle_skill_view(args: serde_json::Value) -> Result<String, String> {
    let skill_name = args["skillName"].as_str().ok_or("skillName required")?;

    for (scope, dir) in find_skill_dirs() {
        let skill_dir = dir.join(skill_name);
        let skill_md = skill_dir.join("SKILL.md");
        if skill_md.exists() {
            let content = fs::read_to_string(&skill_md)
                .map_err(|e| format!("Failed to read SKILL.md: {}", e))?;

            return Ok(format!(
                "## {} ({} scope)\n\n{}",
                skill_name, scope, content
            ));
        }
    }

    Err(format!("Skill '{}' not found in any skill directory.", skill_name))
}

pub async fn handle_skill_manage(args: serde_json::Value) -> Result<String, String> {
    let action = args["action"].as_str().ok_or("action required")?;

    match action {
        "install" => {
            let source = args["source"].as_str().ok_or("source URL/repo required for install")?;

            // Try to install via git clone
            let target_dir = skills_dir().join("__installing__");
            let _ = fs::remove_dir_all(&target_dir);

            let clone = std::process::Command::new("git")
                .args(["clone", "--depth", "1", source, target_dir.to_str().unwrap_or(".")])
                .output();

            match clone {
                Ok(output) if output.status.success() => {
                    // Read the installed skill
                    if let Ok(entries) = fs::read_dir(&target_dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.is_dir() && path.join("SKILL.md").exists() {
                                let name = path.file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("unknown");
                                let dest = skills_dir().join(name);
                                if dest.exists() {
                                    let _ = fs::remove_dir_all(&dest);
                                }
                                fs::rename(&path, &dest)
                                    .map_err(|e| format!("Failed to move skill: {}", e))?;
                                let _ = fs::remove_dir_all(&target_dir);
                                return Ok(format!("Skill installed: {}", name));
                            }
                        }
                    }
                    let _ = fs::remove_dir_all(&target_dir);
                    Ok("Cloned but no skill with SKILL.md found in the repository.".to_string())
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    Err(format!("git clone failed: {}", stderr.trim()))
                }
                Err(e) => Err(format!("Failed to run git clone: {}", e)),
            }
        }
        "create" => {
            let skill_name = args["skillName"].as_str().ok_or("skillName required for create")?;
            let description = args["description"].as_str().unwrap_or("A custom agent skill");

            let skill_dir = skills_dir().join(skill_name);
            if skill_dir.exists() {
                return Err(format!("Skill '{}' already exists at {}", skill_name, skill_dir.display()));
            }

            fs::create_dir_all(&skill_dir)
                .map_err(|e| format!("Failed to create skill directory: {}", e))?;

            let skill_md = format!(
                r#"---
name: {name}
description: {desc}
---

# {name}

## Instructions
Describe what this skill does and how the agent should use it.

## Workflow
1. Step one
2. Step two
3. Step three

## References
- Link to relevant documentation
- Link to example usage
"#,
                name = skill_name,
                desc = description
            );

            fs::write(skill_dir.join("SKILL.md"), skill_md)
                .map_err(|e| format!("Failed to write SKILL.md: {}", e))?;

            Ok(format!("Skill scaffold created: {}\nEdit: {}/SKILL.md",
                skill_name, skill_dir.display()))
        }
        "remove" => {
            let skill_name = args["skillName"].as_str().ok_or("skillName required for remove")?;
            let skill_dir = skills_dir().join(skill_name);

            if !skill_dir.exists() {
                return Err(format!("Skill '{}' not found.", skill_name));
            }

            fs::remove_dir_all(&skill_dir)
                .map_err(|e| format!("Failed to remove skill: {}", e))?;

            Ok(format!("Skill removed: {}", skill_name))
        }
        _ => Err(format!("Unknown action: {}. Use: install, create, remove", action)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_skill_dir() -> PathBuf {
        let pid = std::process::id();
        let mut p = std::env::temp_dir();
        p.push(format!("goblin_skill_test_{}", pid));
        fs::create_dir_all(p.join("test-skill")).unwrap();
        fs::write(
            p.join("test-skill/SKILL.md"),
            "description: A test skill for unit tests\n\n## Overview\n\nContent here\n",
        ).unwrap();
        p
    }

    #[test]
    fn test_defs_exist() {
        assert_eq!(skill_list_def().function.name, "skill_list");
        assert_eq!(skill_view_def().function.name, "skill_view");
        assert_eq!(skill_manage_def().function.name, "skill_manage");
    }

    #[test]
    fn test_extract_description_from_yaml() {
        let dir = temp_skill_dir();
        let desc = extract_description(&dir.join("test-skill").join("SKILL.md"));
        assert_eq!(desc, "A test skill for unit tests");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_extract_description_from_heading() {
        let pid = std::process::id();
        let mut p = std::env::temp_dir();
        p.push(format!("goblin_skill_h2_{}", pid));
        fs::create_dir_all(p.join("h2-skill")).unwrap();
        fs::write(p.join("h2-skill/SKILL.md"), "## My Skill Title\n\nSome body text\n").unwrap();

        let desc = extract_description(&p.join("h2-skill").join("SKILL.md"));
        assert_eq!(desc, "My Skill Title");
        let _ = fs::remove_dir_all(&p);
    }

    #[test]
    fn test_extract_description_missing_file() {
        let desc = extract_description(&PathBuf::from("/nonexistent/skill/path"));
        assert_eq!(desc, "Cannot read SKILL.md");
    }

    #[tokio::test]
    async fn test_skill_list() {
        let result = handle_skill_list(serde_json::json!({})).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("Agent Skills"));
    }

    #[tokio::test]
    async fn test_skill_view_missing() {
        let result = handle_skill_view(serde_json::json!({"skillName": "nonexistent-skill-xyz"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_skill_manage_invalid_action() {
        let result = handle_skill_manage(serde_json::json!({"action": "invalid"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_skill_manage_create_missing_name() {
        let result = handle_skill_manage(serde_json::json!({"action": "create"})).await;
        assert!(result.is_err());
    }
}
