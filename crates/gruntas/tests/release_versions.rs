//! Calendar versions, held at the release workflow's door (R-REL-1, R-REL-3):
//! `scripts/release_tag.sh` is the first step of `.github/workflows/release.yml`,
//! and the step that moves the Action's floating tag names `v0` rather than
//! deriving it from a version whose first number is now a year. The update
//! notice's size rules are `cli/upgrade.rs`'s unit tests.

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn release_tag(tag: &str) -> Option<String> {
    let out = Command::new("bash").arg(root().join("scripts/release_tag.sh")).arg(tag).output().expect("bash runs");
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn r_rel_1_a_release_tag_is_a_calendar_version_with_no_leading_zero() {
    for (tag, version) in [("v2026.11.0", "2026.11.0"), ("v2026.1.3", "2026.1.3"), ("v2026.11.0-rc1", "2026.11.0-rc1"), ("v2027.12.10", "2027.12.10")] {
        assert_eq!(release_tag(tag).as_deref(), Some(version), "{tag} is a release tag");
    }
    for tag in ["v2026.03.0", "v2026.11.01", "v1.2", "v0", "v2026.13.0", "v2026.0.0", "2026.11.0", "v2026.11.0-rc01", "v2026.11.0-beta1", "v26.11.0"] {
        assert_eq!(release_tag(tag), None, "{tag} is refused");
    }
}

#[test]
fn r_rel_3_the_floating_tag_is_v0_and_the_release_starts_at_the_tag_check() {
    let release = std::fs::read_to_string(root().join(".github/workflows/release.yml")).expect("release.yml");
    assert!(
        release.contains("          v=$(scripts/release_tag.sh \"$GITHUB_REF_NAME\")\n          echo \"version=$v\" >> \"$GITHUB_OUTPUT\""),
        "the check job's version comes from the tag check, in a bare assignment so a refused tag fails the step"
    );
    let step = &release[release.find("- name: Move the v0 tag").expect("a step that moves v0")..];
    assert!(step.contains("git/refs/tags/v0\"") && step.contains("\"refs/tags/v0\""), "the step names v0");
    assert!(!step.contains("%%."), "the tag is not derived from the version");
    assert!(step.contains("*-*)") && step.contains("exit 0"), "a prerelease leaves v0 where it is");
}
