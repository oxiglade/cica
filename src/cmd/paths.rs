use anyhow::Result;

use crate::config;

/// Run the paths command
pub fn run() -> Result<()> {
    let paths = config::paths()?;

    println!("Cica data directories:");
    println!();
    println!("  Base:     {}", paths.base.display());
    println!("  Config:   {}", paths.config_file.display());
    println!("  Pairing:  {}", paths.pairing_file.display());
    println!("  Memory:   {}", paths.memory_dir.display());
    println!("  Skills:   {}", paths.skills_dir.display());

    Ok(())
}
