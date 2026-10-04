//! Ctrl-Y: the last answer onto the clipboard, as the model wrote it, its
//! markdown and all — a mouse selection copies it as shown, bullets and
//! a code block's language with it. Two ways, both tried: OSC 52,
//! which the terminal (or a multiplexer passing it on) sets the clipboard
//! from, and the desktop's own clipboard command when there is one.

use std::io::Write;
use std::process::{Command, Stdio};

/// The most sent at once: terminals and multiplexers drop an OSC 52 much
/// past 100 KB of base64, silently.
pub const MAX: usize = 74 * 1024;

/// `ESC ] 52 ; c ; <base64> BEL`.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// The desktop clipboard, when a command for it is on PATH: written on a
/// thread of its own, never waited on.
pub fn system(text: &str) {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if wayland {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    } else if x11 {
        &[("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
    } else {
        &[]
    };
    let text = text.to_string();
    let candidates: Vec<(String, Vec<String>)> = candidates.iter().map(|(c, a)| (c.to_string(), a.iter().map(|s| s.to_string()).collect())).collect();
    std::thread::spawn(move || {
        for (cmd, args) in candidates {
            let Ok(mut child) = Command::new(&cmd).args(&args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { continue };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return;
        }
    });
}

fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_goes_out_as_one_osc_52_sequence() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("héllo\n  code".as_bytes()), "aMOpbGxvCiAgY29kZQ==");
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }
}
