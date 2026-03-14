//! `devenv inputs`: manage inputs in `devenv.yaml`.

use devenv_core::config::Config;
use miette::Result;

pub fn add(name: &str, url: &str, follows: &[String], no_flake: bool) -> Result<()> {
    let mut config = Config::load()?;
    config.add_input(name, url, follows, !no_flake)?;
    config.write()?;
    Ok(())
}
