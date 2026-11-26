//! The carried pi extension: npm/pi bundled into one file
//! (npm/pi/bundle.mjs), embedded here as the Claude plugin is, with the
//! gruntas skill. `gruntas pi extension` (plumbing, out of the main help)
//! writes both into `<gruntas home>/pi` and says where, for the
//! `@gruntas/pi` loader; `gruntas run pi` loads the same files.

use super::Ctx;
use crate::output::Format;
use gruntas_api::{fail, Error};
use serde_json::json;
use std::path::{Path, PathBuf};

/// The bundled extension, as npm/pi/bundle.mjs last built it.
const EXTENSION: &str = include_str!("../../../../npm/pi/carried/gruntas.js");
/// The gruntas skill, the installer's `~/.agents/skills/gruntas` at this version.
const SKILL: &str = include_str!("../../../../skills/gruntas/SKILL.md");
/// What the bundle reports as its version until it is written (bundle.mjs's VERSION_TOKEN).
const VERSION_TOKEN: &str = "0.0.0-gruntas-version";

/// The carried files, written.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Carried {
    /// The extension, for `pi -e`.
    pub extension: PathBuf,
    /// The skill's directory, for `pi --skill`.
    pub skill: PathBuf,
    /// What this call wrote: none when everything was already current.
    pub written: Vec<PathBuf>,
}

/// Where the carried files live: `<gruntas home>/pi`, beside the Claude plugin.
pub(crate) fn dir(env: &dyn Fn(&str) -> String) -> Option<PathBuf> {
    Some(gruntas_harness::launch::plugin_dir(env)?.parent()?.join("pi"))
}

/// Writes the extension (stamped with this gruntas's version) and the skill
/// into `dir`: only a file whose content differs, through a rename, so a pi
/// loading it never reads half a file and an unchanged one is left alone.
pub(crate) fn install(dir: &Path) -> std::io::Result<Carried> {
    let extension = dir.join("gruntas.js");
    let skill = dir.join("skills").join("gruntas");
    let mut written = Vec::new();
    // The token as the JS string literal it is, replaced by another literal.
    let stamped = EXTENSION.replace(&format!("\"{VERSION_TOKEN}\""), &serde_json::Value::from(super::VERSION).to_string());
    for (file, content) in [(extension.clone(), stamped), (skill.join("SKILL.md"), SKILL.to_string())] {
        if std::fs::read(&file).is_ok_and(|had| had == content.as_bytes()) {
            continue;
        }
        let parent = file.parent().unwrap_or(dir);
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(".{}.{}", file.file_name().and_then(|n| n.to_str()).unwrap_or("carried"), std::process::id()));
        if let Err(e) = std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, &file)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        written.push(file);
    }
    Ok(Carried { extension, skill, written })
}

/// `gruntas pi extension`: writes the carried files and prints where they are.
pub(super) fn extension(ctx: &mut Ctx) -> Result<(), Error> {
    let dir = dir(ctx.io.env).ok_or_else(|| fail("no_home", "there is no home directory to keep the pi extension in — set HOME"))?;
    let c = install(&dir).map_err(|e| fail("write_failed", format!("the pi extension could not be written to {}: {e}", dir.display())))?;
    if ctx.format != Format::Human {
        let summary = format!("pi extension {} at {}", super::VERSION, c.extension.display());
        let data = json!({ "extension": c.extension, "skill": c.skill, "version": super::VERSION, "written": c.written });
        return super::sessions::emit_data(ctx, data, summary);
    }
    let _ = writeln!(ctx.io.stdout, "extension  {}\nskill      {}", c.extension.display(), c.skill.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_carried_files_are_written_once_and_stamped_with_this_version() {
        let dir = std::env::temp_dir().join(format!("gruntas-pi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = install(&dir).unwrap();
        assert_eq!(first.written, vec![dir.join("gruntas.js"), dir.join("skills/gruntas/SKILL.md")]);
        let js = std::fs::read_to_string(&first.extension).unwrap();
        assert!(!js.contains(VERSION_TOKEN) && js.contains(&format!("\"{}\"", super::super::VERSION)));
        assert_eq!(std::fs::read_to_string(first.skill.join("SKILL.md")).unwrap(), SKILL);
        let again = install(&dir).unwrap();
        assert_eq!((again.extension, again.skill, again.written), (first.extension, first.skill.clone(), vec![]));
        std::fs::write(first.skill.join("SKILL.md"), "old").unwrap();
        assert_eq!(install(&dir).unwrap().written, vec![dir.join("skills/gruntas/SKILL.md")]);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "no temporary file left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_embedded_bundle_carries_the_version_token_once_stamped_away() {
        assert!(EXTENSION.starts_with("// @gruntas/pi carried extension: "));
        assert!(EXTENSION.contains(VERSION_TOKEN));
    }
}
