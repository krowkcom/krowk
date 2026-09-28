//! The branch read where the agent is at work. Its own binary: the `git`
//! it runs would hold the unit tests' sockets open across a fork.

use krowk_tui::pr::look;
use std::process::Command;

#[test]
fn the_branch_is_the_first_directory_in_a_repository() {
    let base = std::env::temp_dir().join(format!("krowk-pr-look-{}", std::process::id()));
    let (repo, bare) = (base.join("repo"), base.join("bare"));
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| assert!(Command::new("git").args(args).current_dir(&repo).output().unwrap().status.success());
    git(&["init", "-q", "-b", "feature/x"]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "x"]);
    assert_eq!(look(&[base.join("gone"), bare.clone(), repo.clone()], false), ("feature/x".to_string(), None), "a removed worktree, then no repository, then the repository");
    assert_eq!(look(&[bare], false), (String::new(), None));
    let _ = std::fs::remove_dir_all(&base);
}
