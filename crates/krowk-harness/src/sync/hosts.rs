//! `krowk hosts` (R-NET-4): the machines of yours that host synced
//! sessions, found with no step in the Tailscale admin console. The
//! registry's sessions say which of your devices host — each sealed index
//! names the machine that last took its session up, and the lease which one
//! holds it now — your verified device list says which devices are yours
//! and what they are called, and the tailnet says which are reachable.
//! Nothing here reads a tag: a tagged machine is the tag's in Tailscale's
//! eyes, not yours, so the same-user check could never pass it.
//!
//! A machine also says so itself (R-HOST-3): each host daemon's sealed
//! announcement, opened here, lists it — hosting nothing, or with no
//! Tailscale — online or offline since it was last seen, with its version,
//! instances and directories.

use super::tailscale::{Node, Status, Tailnet};
use super::viewer::Listed;
use krowk_client::device_chain::{Chain, Kind};
use krowk_client::presence::Announcement;

/// A host's announcement, opened, as the registry lists it.
#[derive(Debug, Clone)]
pub struct Announced {
    /// The registry lists it online now, by its own clock.
    pub online: bool,
    /// When it was last refreshed: last seen.
    pub updated_at: String,
    pub announcement: Announcement,
}

/// One machine of yours that hosts synced sessions.
#[derive(Debug, Clone)]
pub struct Host {
    /// Its device id, as the device list has it.
    pub device: String,
    /// The name and OS it was added to the device list with.
    pub name: String,
    pub os: String,
    /// It is the machine asking.
    pub this_machine: bool,
    /// It holds the lease of one of its sessions now: hosting.
    pub hosting: bool,
    /// How many synced sessions are its: held now, else last taken up.
    pub sessions: usize,
    /// Its tailnet node, as it last kept it in a session's index: none
    /// when it hosted with Tailscale down, or before it kept one.
    pub tailnet: Option<Tailnet>,
    /// That node among this machine's tailnet peers, when Tailscale here
    /// answered and finds it.
    pub peer: Option<Node>,
    /// What it last announced of itself, if it did.
    pub announced: Option<Announced>,
}

impl Host {
    /// Reachable directly: on this tailnet and online there. Never this
    /// machine, which is no peer of its own.
    pub fn direct(&self) -> bool {
        self.peer.as_ref().is_some_and(|p| p.online)
    }
}

/// The hosts among `listed` (most recently written first, as
/// `viewer::list` gives them): each session under the lease's holder while
/// one hosts it, else under the machine its index names; a session with
/// neither hosts nowhere. A session counts only when a device on the list
/// now signed its record, so one a removed device made — under a key
/// generation it still holds — names nobody's machine. Only devices on
/// `chain` are listed, the recovery device never, so a removed machine and
/// anyone else's are not. `me` is this device; `status` this machine's
/// tailnet, none when Tailscale here did not answer. Each of `announced`
/// (opened with `presence::open`, so its device on the list) lists its
/// device too, hosting nothing or not.
pub fn hosts(listed: &[Listed], announced: &[Announced], chain: &Chain, me: &str, status: Option<&Status>) -> Vec<Host> {
    let device_of = |id: &str| chain.devices().iter().find(|d| d.kind == Kind::Device && d.id().to_string() == id);
    let mut out: Vec<Host> = Vec::new();
    for s in listed.iter().filter(|s| device_of(&s.signer).is_some()) {
        let kept = s.index.host.as_ref();
        let Some(device) = s.holder.clone().or_else(|| kept.map(|h| h.device.clone())) else { continue };
        // The node only from the machine's own claim, never another's.
        let kept = kept.filter(|k| k.device == device);
        let Some(d) = device_of(&device) else { continue };
        let at = match out.iter().position(|h| h.device == device) {
            Some(at) => at,
            None => {
                out.push(Host { device: device.clone(), name: d.name.clone(), os: d.os.clone(), this_machine: device == me, hosting: false, sessions: 0, tailnet: None, peer: None, announced: None });
                out.len() - 1
            }
        };
        let h = &mut out[at];
        h.sessions += 1;
        h.hosting |= s.holder.as_deref() == Some(device.as_str());
        // The newest session's node: the one it kept most recently.
        if h.tailnet.is_none() {
            h.tailnet = kept.and_then(|k| k.tailnet.clone());
        }
    }
    for a in announced {
        let device = &a.announcement.device;
        let Some(d) = device_of(device) else { continue };
        match out.iter_mut().find(|h| h.device == *device) {
            Some(h) => h.announced = Some(a.clone()),
            None => out.push(Host { device: device.clone(), name: d.name.clone(), os: d.os.clone(), this_machine: device == me, hosting: false, sessions: 0, tailnet: None, peer: None, announced: Some(a.clone()) }),
        }
    }
    for h in &mut out {
        h.peer = status.zip(h.tailnet.as_ref()).filter(|_| !h.this_machine).and_then(|(s, t)| s.peer_for(t)).cloned();
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.device.cmp(&b.device)));
    out
}

/// Whether the machine that announced `a` would take a task in the
/// repository with normalised origin `origin` (R-ALLOC-2, D7): none when
/// ready, else the fix, in the order a person meets them: `krowk machines
/// accept on` when it is not accepting, `run: git clone <clone_url>` when it
/// has no clone, and the instance's own fix line when no account there is
/// ready. `clone_url` is the origin as this machine's clone has it; the
/// fix shows it with any user or password left out.
pub fn fix(a: &Announcement, origin: &str, clone_url: &str) -> Option<String> {
    if !a.accepting {
        return Some("krowk machines accept on".into());
    }
    if !a.repositories.iter().any(|r| r.origin == origin) {
        return Some(format!("run: git clone {}", without_credentials(clone_url)));
    }
    if a.instances.iter().any(|i| i.ready) {
        return None;
    }
    Some(match a.instances.first() {
        None => "sign in an account: run krowk connect there".into(),
        Some(i) => i.fix.clone().map_or_else(|| format!("{} is not ready ({})", i.name, i.state), |f| format!("{}: {f}", i.name)),
    })
}

/// `url` with no user, password, query or fragment in it: an http(s) or git
/// URL loses its whole userinfo, an ssh one (`ssh://`, `git+ssh://`) keeps
/// the user alone (`git@`), which names no secret. A scp-style
/// `git@host:path` has only that user and stays as it is.
fn without_credentials(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let Some((userinfo, host)) = authority.rsplit_once('@') else { return url.to_string() };
    let user = scheme.to_ascii_lowercase().contains("ssh").then(|| userinfo.split(':').next().filter(|u| !u.is_empty()).map(|u| format!("{u}@"))).flatten();
    let slash = if rest.contains('/') { "/" } else { "" };
    format!("{scheme}://{}{host}{slash}{path}", user.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slice 03 (R-ALLOC-2, D7): for the current repository each machine
    /// is ready, or says the fix.
    #[test]
    fn alloc_03_each_machine_is_ready_or_says_its_fix() {
        use krowk_client::presence::{AnnouncedInstance, AnnouncedRepository};
        let ready = AnnouncedInstance { name: "anthropic".into(), kind: "anthropic".into(), ready: true, state: "ready".into(), fix: None, models: Vec::new(), limit: None };
        let a = Announcement { device: "d".into(), name: "desktop".into(), os: "linux".into(), version: "dev".into(), online_since_ms: 0, instances: vec![ready.clone()], directories: vec!["/w/app".into()], sessions: Vec::new(), capacity: None, repositories: vec![AnnouncedRepository { directory: "/w/app".into(), origin: "github.com/acme/app".into() }], accepting: true };
        let (origin, url) = ("github.com/acme/app", "git@github.com:acme/app.git");
        assert_eq!(fix(&a, origin, url), None, "ready");
        let unclone = Announcement { repositories: Vec::new(), ..a.clone() };
        assert_eq!(fix(&unclone, origin, url).as_deref(), Some("run: git clone git@github.com:acme/app.git"));
        assert_eq!(fix(&unclone, origin, "https://ann:ghp_secret@github.com/acme/app.git").as_deref(), Some("run: git clone https://github.com/acme/app.git"), "never a credential");
        let off = Announcement { accepting: false, ..a.clone() };
        assert_eq!(fix(&off, origin, url).as_deref(), Some("krowk machines accept on"));
        let signed_out = Announcement { instances: vec![AnnouncedInstance { ready: false, state: "loggedOut".into(), fix: Some("run claude /login".into()), ..ready }], ..a };
        assert!(fix(&signed_out, origin, url).unwrap().contains("claude /login"));
    }

    /// The fix names no secret, in any way a URL carries one, and a machine
    /// with no account at all is told to sign one in.
    #[test]
    fn a_fix_never_carries_a_credential_and_a_machine_with_no_account_says_so() {
        use krowk_client::presence::AnnouncedRepository;
        let a = Announcement { device: "d".into(), name: "desktop".into(), os: "linux".into(), version: "dev".into(), online_since_ms: 0, instances: Vec::new(), directories: Vec::new(), sessions: Vec::new(), capacity: None, repositories: vec![AnnouncedRepository { directory: "/w".into(), origin: "github.com/acme/app".into() }], accepting: true };
        assert_eq!(fix(&a, "github.com/acme/app", "u").as_deref(), Some("sign in an account: run krowk connect there"));
        for (url, shown) in [
            ("https://tok@github.com/acme/app.git", "https://github.com/acme/app.git"),
            ("https://ann:p%40ss@github.com:8443/acme/app", "https://github.com:8443/acme/app"),
            ("ssh://git:pw@github.com/acme/app.git", "ssh://git@github.com/acme/app.git"),
            ("ssh://git@github.com/acme/app.git", "ssh://git@github.com/acme/app.git"),
            ("git+ssh://git@github.com/acme/app.git", "git+ssh://git@github.com/acme/app.git"),
            ("https://github.com/acme/app.git?access_token=x#frag", "https://github.com/acme/app.git"),
            ("git@github.com:acme/app.git", "git@github.com:acme/app.git"),
            ("https://github.com/acme/app.git", "https://github.com/acme/app.git"),
        ] {
            assert_eq!(without_credentials(url), shown);
        }
    }
    use crate::sync::store::{HostedOn, Index};
    use krowk_client::device_chain::{Change, Subject};
    use krowk_client::e2e::{DeviceKey, SigningKey};

    fn subject(name: &str, kind: Kind) -> (Subject, SigningKey) {
        let (d, s) = (DeviceKey::generate(), SigningKey::generate());
        (Subject { kind, name: name.into(), os: "linux".into(), device: d.public(), signing: s.public() }, s)
    }

    /// A list of `laptop` (which starts it, with the recovery device),
    /// `server`, and `gone`, added and then removed.
    fn chain() -> (Chain, [String; 4]) {
        let ((laptop, signing), (recovery, recovery_key)) = (subject("laptop", Kind::Device), subject("recovery", Kind::Recovery));
        let (server, gone) = (subject("server", Kind::Device).0, subject("gone", Kind::Device).0);
        let ids = [laptop.id(), server.id(), recovery.id(), gone.id()].map(|d| d.to_string());
        let (chain, start) = Chain::start(laptop.clone(), &signing, Some((recovery, &recovery_key)), 1).unwrap();
        let (chain, _) = chain.batch(&start.newest, vec![Change::Add(server), Change::Add(gone.clone())], laptop.id(), &signing, 2).unwrap();
        let (chain, _) = chain.batch(&start.newest, vec![Change::Remove(gone)], laptop.id(), &signing, 3).unwrap();
        (chain, ids)
    }

    /// A session whose record `signer` signed, whose index names `host`,
    /// and whose lease `holder` holds.
    fn listed(signer: &str, host: Option<(&str, Option<Tailnet>)>, holder: Option<&str>) -> Listed {
        let index = Index { host: host.map(|(device, tailnet)| HostedOn { device: device.into(), tailnet }), ..Index::default() };
        Listed { id: "s".into(), index, holder: holder.map(String::from), signer: signer.into() }
    }

    fn node(name: &str, ip: &str) -> Tailnet {
        Tailnet { name: name.into(), dns_name: format!("{name}.tail1.ts.net"), ips: vec![ip.parse().unwrap()] }
    }

    /// R-NET-4: every machine of yours that hosts is listed by its device
    /// list name, with how many sessions and whether it hosts now; one that
    /// hosts nothing, a removed one and the recovery device are not.
    #[test]
    fn r_net_4_hosts_are_your_devices_that_host_by_their_list_names() {
        let (chain, [laptop, server, recovery, gone]) = chain();
        let sessions = [
            listed(&laptop, Some((&server, Some(node("srv", "100.64.0.2")))), Some(&server)),
            listed(&laptop, Some((&server, None)), None),
            listed(&laptop, None, Some(&laptop)),
            listed(&laptop, None, None),
            listed(&laptop, Some((&gone, None)), None),
            listed(&laptop, Some((&recovery, None)), None),
            listed(&laptop, Some(("ffffffffffffffffffffffffffffffff", None)), None),
        ];
        let got = hosts(&sessions, &[], &chain, &laptop, None);
        let rows: Vec<_> = got.iter().map(|h| (h.name.as_str(), h.sessions, h.hosting, h.this_machine)).collect();
        assert_eq!(rows, [("laptop", 1, true, true), ("server", 2, true, false)]);
        assert_eq!(got[1].tailnet, Some(node("srv", "100.64.0.2")), "the newest session's node");
        assert!(got.iter().all(|h| h.peer.is_none() && !h.direct()), "no tailnet asked");
    }

    /// R-NET-4: a session a removed device signed — made under a key
    /// generation it still holds — names nobody's machine, though its index
    /// names a listed one and an address for it.
    #[test]
    fn r_net_4_a_session_a_removed_device_signed_names_no_host() {
        let (chain, [laptop, server, _, gone]) = chain();
        let planted = listed(&gone, Some((&server, Some(node("evil", "100.64.6.6")))), None);
        assert!(hosts(std::slice::from_ref(&planted), &[], &chain, &laptop, None).is_empty());
        let got = hosts(&[planted, listed(&laptop, Some((&server, Some(node("srv", "100.64.0.2")))), None)], &[], &chain, &laptop, None);
        assert_eq!(got[0].tailnet, Some(node("srv", "100.64.0.2")), "the planted node is never shown");
    }

    /// R-NET-4: while a lease is held, the session is its holder's — though
    /// the index still names the machine before it, which a new holder
    /// rewrites only at its first write — and the node is taken from the
    /// holder's own claim alone.
    #[test]
    fn r_net_4_a_held_session_is_its_holders_and_its_node_only_its_own_claim() {
        let (chain, [laptop, server, ..]) = chain();
        let got = hosts(&[listed(&laptop, Some((&laptop, Some(node("lap", "100.64.0.1")))), Some(&server))], &[], &chain, &laptop, None);
        assert_eq!(got.iter().map(|h| (h.name.as_str(), h.hosting)).collect::<Vec<_>>(), [("server", true)]);
        assert_eq!(got[0].tailnet, None, "the laptop's node is not the server's");
    }

    /// R-NET-4: a host is matched to this tailnet's peers by the node it
    /// kept, and is direct only when that peer is online; a node on no
    /// tailnet this machine is on is no peer.
    #[test]
    fn r_net_4_a_host_is_direct_when_its_kept_node_is_an_online_peer() {
        let (chain, [laptop, server, ..]) = chain();
        let status: Status = serde_json::from_value(serde_json::json!({"BackendState": "Running", "Self": {"HostName": "laptop", "DNSName": "laptop.tail1.ts.net.", "TailscaleIPs": ["100.64.0.1"]},
            "Peer": {"k": {"HostName": "srv", "DNSName": "srv.tail1.ts.net.", "TailscaleIPs": ["100.64.0.2"], "Online": true}}})).unwrap();
        let on = |t: Tailnet| hosts(&[listed(&laptop, Some((&server, Some(t))), None)], &[], &chain, &laptop, Some(&status));

        let got = on(node("srv", "100.64.0.2"));
        assert!(got[0].direct() && !got[0].hosting, "{got:?}");
        let elsewhere = Tailnet { name: "srv".into(), dns_name: "srv.tail2.ts.net".into(), ips: vec!["100.64.0.2".parse().unwrap()] };
        assert!(!on(elsewhere)[0].direct(), "another tailnet's node at the same address");

        let mut off = status.clone();
        off.peer.as_mut().unwrap().get_mut("k").unwrap().online = false;
        let got = hosts(&[listed(&laptop, Some((&server, Some(node("srv", "100.64.0.2")))), None)], &[], &chain, &laptop, Some(&off));
        assert!(got[0].peer.is_some() && !got[0].direct(), "on the tailnet, offline there");
    }
    fn announced(device: &str, online: bool) -> Announced {
        let announcement = Announcement { device: device.into(), name: "claimed".into(), os: "linux".into(), version: "0.31.0".into(), online_since_ms: 0, instances: Vec::new(), directories: vec!["/work".into()], sessions: Vec::new(), capacity: None, repositories: Vec::new(), accepting: false };
        Announced { online, updated_at: "2026-10-25T10:00:00Z".into(), announcement }
    }

    /// R-HOST-3, R-NET-4: a device that announced itself is listed though it
    /// hosts nothing, by its list name; one that hosts carries its
    /// announcement; a removed device's, the recovery device's and a
    /// stranger's are never listed.
    #[test]
    fn r_host_3_a_device_that_announced_is_listed_though_it_hosts_nothing() {
        let (chain, [laptop, server, recovery, gone]) = chain();
        let sessions = [listed(&laptop, Some((&laptop, None)), Some(&laptop))];
        let seen = [announced(&server, true), announced(&laptop, false), announced(&gone, true), announced(&recovery, true), announced("ffffffffffffffffffffffffffffffff", true)];
        let got = hosts(&sessions, &seen, &chain, &laptop, None);
        let rows: Vec<_> = got.iter().map(|h| (h.name.as_str(), h.sessions, h.hosting, h.announced.as_ref().map(|a| a.online))).collect();
        assert_eq!(rows, [("laptop", 1, true, Some(false)), ("server", 0, false, Some(true))]);
        assert_eq!(got[1].announced.as_ref().unwrap().announcement.directories, ["/work"]);
    }
}
