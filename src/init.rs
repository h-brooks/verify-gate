use crate::config::INIT_TEMPLATE;
use anyhow::Result;
use std::path::Path;

/// Implements the `init` subcommand: writes `.verify-gate.toml` (if it
/// doesn't already exist) and prints the settings.json snippet to add.
pub fn run_init() -> Result<()> {
    let path = Path::new(".verify-gate.toml");
    if path.exists() {
        eprintln!(
            "verify-gate: {} already exists, not overwriting",
            path.display()
        );
    } else {
        std::fs::write(path, INIT_TEMPLATE)?;
        println!("wrote {}", path.display());
    }
    println!();
    println!("Add this to your .claude/settings.json:");
    println!();
    println!("{}", settings_snippet());
    Ok(())
}

pub fn settings_snippet() -> String {
    let snippet = serde_json::json!({
        "hooks": {
            "Stop": [
                {
                    "hooks": [
                        { "type": "command", "command": "verify-gate hook" }
                    ]
                }
            ]
        }
    });
    serde_json::to_string_pretty(&snippet).unwrap()
}
