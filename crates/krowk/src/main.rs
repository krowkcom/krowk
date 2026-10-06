use std::io::IsTerminal;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The paste guard, as the hook Codex runs: its input on stdin, its
    // answer on stdout, and nothing of the CLI around it.
    #[cfg(feature = "harness")]
    if args.first().map(String::as_str) == Some(krowk_harness::paste_guard::HOOK_ARG) {
        let mut input = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
        println!("{}", krowk_harness::paste_guard::hook_main(&input));
        return;
    }
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    // stderr is not held locked: the spinner draws on it from its own thread,
    // and a lock held here for the whole run would leave that thread blocked on
    // its first frame and the command waiting on the thread forever.
    let (mut stdout, mut stderr) = (std::io::stdout().lock(), std::io::stderr());
    let (tty, err_tty) = (std::io::stdout().is_terminal(), std::io::stderr().is_terminal());
    // Only the full build asks: stdin matters to nothing but its TUI.
    let stdin_tty = cfg!(feature = "harness") && std::io::stdin().is_terminal();
    let mut io = krowk::cli::Io { stdout: &mut stdout, stderr: &mut stderr, env: &env, tty, err_tty, stdin_tty };
    std::process::exit(krowk::cli::run(&args, &mut io));
}
