use std::path::PathBuf;

/// Returns the base Claude directory (~/.claude or RECALL_ECHO_HOME override).
pub fn claude_dir() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("RECALL_ECHO_HOME") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or("Could not determine home directory")?;
    Ok(home.join(".claude"))
}

pub fn memories_dir() -> Result<PathBuf, String> {
    Ok(claude_dir()?.join("memories"))
}

pub fn memory_file() -> Result<PathBuf, String> {
    Ok(claude_dir()?.join("memory").join("MEMORY.md"))
}

pub fn ephemeral_file() -> Result<PathBuf, String> {
    Ok(claude_dir()?.join("EPHEMERAL.md"))
}

pub fn archive_index() -> Result<PathBuf, String> {
    Ok(claude_dir()?.join("ARCHIVE.md"))
}

pub fn settings_file() -> Result<PathBuf, String> {
    Ok(claude_dir()?.join("settings.json"))
}

pub fn rules_dir() -> Result<PathBuf, String> {
    Ok(claude_dir()?.join("rules"))
}

pub fn protocol_file() -> Result<PathBuf, String> {
    Ok(rules_dir()?.join("recall-echo.md"))
}

pub fn conversations_dir() -> Result<PathBuf, String> {
    Ok(claude_dir()?.join("conversations"))
}

pub fn config_file() -> Result<PathBuf, String> {
    Ok(claude_dir()?.join(".recall-echo.toml"))
}
