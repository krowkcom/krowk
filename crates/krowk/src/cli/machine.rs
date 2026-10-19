//! What this machine is called: on the workspace's device list, and on the
//! browser login that names it and the key that login mints. One rule for
//! both, in the lean build too, where `krowk login` lives.

use super::Ctx;

/// `--name`, else KROWK_DEVICE_NAME, else its host name, which is what a
/// person recognises it by — and which the registry sees in the clear, so
/// either of the first two keeps it from going there. Control characters
/// are dropped: the name is printed on other machines' terminals.
/// `--name` is the sync commands' flag, so the lean build has none.
pub(super) fn name(ctx: &Ctx) -> String {
    #[cfg(feature = "harness")]
    let given = ctx.f.name.clone();
    #[cfg(not(feature = "harness"))]
    let given = String::new();
    let chosen = if given.is_empty() { ctx.env("KROWK_DEVICE_NAME") } else { given };
    let name = if chosen.trim().is_empty() { host_name() } else { chosen };
    printable(&name)
}

/// `s` without control characters: what a registry sends back is printed
/// here, and an escape sequence in a name must not reach the terminal.
pub(super) fn printable(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()
}

fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length, and gethostname
        // writes at most that many bytes.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    "krowk device".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name the registry hands back is printed here; an escape sequence in
    /// one never reaches the terminal.
    #[test]
    fn names_are_printed_without_control_characters() {
        assert_eq!(printable("laptop\x1b[31m\u{7}\n"), "laptop[31m");
        assert_eq!(printable("  work laptop "), "work laptop");
    }
}
