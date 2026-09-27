//! krowk over MCP stdio. Reads its key the way the CLI does — KROWK_TOKEN,
//! then the workspace the root's config names — and serves without one when
//! that key cannot be had: reads still work, writes are refused rather than
//! landing in the anonymous workspace the config was written to avoid.

use krowk::mcp::Server;
use std::io::Write;

const USAGE: &str = "Usage of krowk-mcp:
  -root string
    \tonly upload files under this directory (default: the working directory)
  -version
    \tprint the version and exit
";

fn main() {
    let mut root = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let body = arg.trim_start_matches('-');
        let (name, inline) = body.split_once('=').map_or((body, None), |(n, v)| (n, Some(v.to_string())));
        match name {
            "version" if arg.starts_with('-') => {
                println!("{}", krowk::cli::VERSION);
                return;
            }
            "root" if arg.starts_with('-') => match inline.or_else(|| args.next()) {
                Some(v) => root = v,
                None => fail("flag needs an argument: -root"),
            },
            "h" | "help" if arg.starts_with('-') => {
                eprint!("{USAGE}");
                return;
            }
            _ if arg.starts_with('-') => fail(&format!("flag provided but not defined: -{name}")),
            _ => {}
        }
    }
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    if root.is_empty() {
        root = env("KROWK_MCP_ROOT");
    }

    // A home krowk cannot use is no key, and says why, rather than a key
    // read from around it.
    let unusable = krowk_api::home::dir(&env).err().filter(|e| e.code() != "no_home");
    let (token, workspace_err) = match unusable.map(Err).unwrap_or_else(|| krowk::config::load(&root, &env, "").map_err(|e| krowk_api::fail("bad_config", e))) {
        Err(e) => (String::new(), Some(e)),
        Ok(cfg) => match krowk_api::creds::resolve_token(&env, &cfg.workspace) {
            Ok(t) => (t, None),
            Err(e) => (String::new(), Some(e)),
        },
    };
    if let Some(e) = &workspace_err {
        let reason = if e.fix().is_empty() { e.code() } else { format!("{} — {}", e.code(), e.fix()) };
        eprintln!("krowk-mcp: {reason} — serving without a key: reads still work, uploads and claims are refused");
    }
    let server = Server {
        client: krowk_api::Client::new(&krowk_api::base_url_for(false, &env), &token),
        env: &env,
        version: krowk::cli::VERSION.to_string(),
        root,
        workspace_err,
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = server.serve(stdin.lock(), &mut stdout) {
        let _ = stdout.flush();
        eprintln!("krowk-mcp: {e}");
        std::process::exit(1);
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}\n{USAGE}");
    std::process::exit(2);
}
