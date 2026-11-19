//! Automatic allocation end to end (slice 10): the CI case. Machine A has
//! every build slot in use; machine B is idle; both have a clone of the
//! same origin, and A is on a local branch with an unpushed commit and an
//! uncommitted change. A task handed over without a host runs on B from
//! A's exact state, and its changes land in A's working tree.
//!
//! What the contract (slice 01) can hold already is the data path,
//! through the placement, job and result interfaces, with no registry
//! between the machines; slice 10 runs it across two in-process hosts
//! against the in-memory registry, through krowk's own subagent tool and
//! through krowk-mcp, and makes these pass.

#![cfg(unix)]

use krowk_client::host_command::ModelChoice;
use krowk_client::presence::{AnnouncedInstance, AnnouncedRepository, Announcement, Capacity, Slots};
use krowk_harness::sync::handoff::{self, Bundle};
use krowk_harness::sync::placement::{self, Ask, Candidate, Failure, Pick};
use krowk_harness::worktree::apply::Applied;
use std::path::{Path, PathBuf};

const ORIGIN: &str = "github.com/acme/app";

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("krowk-alloc-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"]).args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A machine as its announcement says, with `in_use` of its 3 build slots
/// taken.
fn machine(name: &str, here: bool, builds_in_use: u32) -> Candidate {
    let capacity = Capacity { cores: 8, load_1m: 0.5, free_memory: 16 << 30, build_slots: Slots { in_use: builds_in_use, total: 3 }, agent_slots: Slots { in_use: 0, total: 8 } };
    let instance = AnnouncedInstance { name: "anthropic".into(), kind: "anthropic".into(), ready: true, state: "ready".into(), fix: None, models: vec!["claude-opus-5-5".into()], limit: None };
    let announcement = Announcement {
        device: format!("{name}-id"),
        name: name.into(),
        os: "linux".into(),
        version: "dev".into(),
        online_since_ms: 0,
        instances: vec![instance],
        directories: vec![format!("/{name}/app")],
        sessions: Vec::new(),
        capacity: Some(capacity),
        repositories: vec![AnnouncedRepository { directory: format!("/{name}/app"), origin: ORIGIN.into() }],
        accepting: true,
    };
    Candidate { announcement, online: true, here, placed_recently: 0 }
}

/// Slice 10: with A's build slots full the task goes to B, from A's exact
/// state, and B's changes land in A's working tree, A's index and HEAD
/// untouched.
#[test]
#[ignore = "slice 10: the CI case end to end"]
fn alloc_10_with_this_machine_busy_the_task_runs_on_the_other_and_lands_here() {
    let model = ModelChoice::default();
    let ask = Ask { origin: ORIGIN, clone_url: "https://github.com/acme/app.git", model: &model, host: None };
    let choice = placement::choose(&[machine("a", true, 3), machine("b", false, 0)], &ask).unwrap();
    assert_eq!(choice.picks[0], Pick::There(1));
    assert_eq!(choice.why, "this machine is busy (3 of 3 build slots in use)");

    let root = scratch("ci");
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q"]);
    std::fs::write(seed.join("lib.rs"), "fn a() {}\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "base"]);
    git(&root, &["clone", "-q", "--bare", "seed", "origin.git"]);
    git(&root, &["clone", "-q", "origin.git", "a"]);
    git(&root, &["clone", "-q", "origin.git", "b"]);
    let (a, b) = (root.join("a"), root.join("b"));
    git(&a, &["checkout", "-q", "-b", "local"]);
    std::fs::write(a.join("unpushed.rs"), "fn u() {}\n").unwrap();
    git(&a, &["add", "."]);
    git(&a, &["commit", "-q", "-m", "unpushed"]);
    std::fs::write(a.join("lib.rs"), "fn a() {}\nfn uncommitted() {}\n").unwrap();
    let head = git(&a, &["rev-parse", "HEAD"]);

    let session = "0190f3c2-0000-7000-8000-00000000000a";
    let job = Bundle::decode(&handoff::pack_job(&a, session).unwrap().encode(), session).unwrap();
    let on_b = handoff::apply_job(&job, &b, &root.join("b-worktrees")).unwrap();
    let wt = on_b.worktree.path.clone();
    assert_eq!(std::fs::read_to_string(wt.join("lib.rs")).unwrap(), "fn a() {}\nfn uncommitted() {}\n", "B starts from A's exact state");
    assert!(wt.join("unpushed.rs").exists());
    std::fs::write(wt.join("fixed.rs"), "fn fixed() {}\n").unwrap();

    let child = "0190f3c2-0000-7000-8000-00000000000b";
    let result = Bundle::decode(&handoff::pack_result(&wt, &job.base, child).unwrap().encode(), child).unwrap();
    let (_, landed) = handoff::apply_result(&result, &a, &root.join("a-worktrees")).unwrap();
    assert!(matches!(&landed, Applied::Applied(files) if files == &["fixed.rs".to_string()]), "{landed:?}");
    assert_eq!(std::fs::read_to_string(a.join("fixed.rs")).unwrap(), "fn fixed() {}\n", "B's change is in A's working tree");
    assert_eq!(std::fs::read_to_string(a.join("lib.rs")).unwrap(), "fn a() {}\nfn uncommitted() {}\n", "A's own uncommitted change is kept");
    assert_eq!(git(&a, &["rev-parse", "HEAD"]), head);
    let _ = std::fs::remove_dir_all(&root);
}

/// Slice 10 (D11, R-ALLOC-9): with B refusing there is a third machine to
/// move to, once; a machine dropping mid-run fails with the message.
#[test]
#[ignore = "slice 10: refusal and a dropped machine end to end"]
fn alloc_10_a_refusal_moves_on_once_and_a_dropped_machine_fails_plainly() {
    let model = ModelChoice::default();
    let ask = Ask { origin: ORIGIN, clone_url: "https://github.com/acme/app.git", model: &model, host: None };
    let choice = placement::choose(&[machine("a", true, 3), machine("b", false, 0), machine("c", false, 1)], &ask).unwrap();
    assert_eq!(choice.picks, [Pick::There(1), Pick::There(2)], "B, then C once");
    assert_eq!(Failure::Offline { machine: "b".into() }.to_string(), "b went offline mid-task; its work is in a worktree there.");
}
