//! pairdeck: a modern twin-panel file manager for the terminal.

use anyhow::Result;
use app::{App, AppEvent, External};
use crossbeam_channel::RecvTimeoutError;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use pairdeck::{app, config, ui, util, vfs};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io::{Stdout, Write, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

type Term = Terminal<CrosstermBackend<Stdout>>;

struct Args {
    dirs: [Option<PathBuf>; 2],
    last_dir_file: Option<PathBuf>,
    no_mouse: bool,
    no_images: bool,
    image_info: bool,
}

fn parse_args() -> Args {
    let mut a = Args { dirs: [None, None], last_dir_file: None, no_mouse: false, no_images: false, image_info: false };
    let mut it = std::env::args().skip(1);
    let mut n = 0;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{}", pairdeck::cli::help_text());
                std::process::exit(0);
            }
            "--man" => {
                print!("{}", pairdeck::cli::man_page());
                std::process::exit(0);
            }
            "--completions" => {
                let shell = it.next().unwrap_or_default();
                match pairdeck::cli::completions(&shell) {
                    Some(script) => {
                        print!("{script}");
                        std::process::exit(0);
                    }
                    None => {
                        eprintln!("pdeck: --completions needs one of: {}", pairdeck::cli::SHELLS.join(", "));
                        std::process::exit(2);
                    }
                }
            }
            "-V" | "--version" => {
                println!("pairdeck {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--sample-config" => {
                print!("{}", config::SAMPLE_CONFIG);
                std::process::exit(0);
            }
            "-P" | "--printwd" => a.last_dir_file = it.next().map(PathBuf::from),
            "-d" | "--nomouse" => a.no_mouse = true,
            "--no-images" => a.no_images = true,
            "--image-info" => a.image_info = true,
            other => {
                if n < 2 {
                    let p = PathBuf::from(util::expand_tilde(other));
                    let p = std::fs::canonicalize(&p).unwrap_or(p);
                    a.dirs[n] = Some(p);
                    n += 1;
                }
            }
        }
    }
    a
}

fn setup_terminal(mouse: bool) -> Result<Term> {
    terminal::enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
    }
    if matches!(terminal::supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore_terminal() {
    let mut out = stdout();
    if matches!(terminal::supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste, LeaveAlternateScreen, crossterm::cursor::Show);
    let _ = terminal::disable_raw_mode();
}

#[allow(deprecated)] // from_fontsize: we compute the font size ourselves via ioctl
fn make_picker(cfg: &config::Config, disabled: bool) -> Option<ratatui_image::picker::Picker> {
    use ratatui_image::{
        FontSize,
        picker::{Picker, ProtocolType},
    };
    if disabled || !cfg.preview_images {
        return None;
    }
    // Font size from the window pixel size (no stdin round trip needed).
    let font_from_ioctl = || {
        terminal::window_size().ok().and_then(|w| {
            (w.width > 0 && w.height > 0 && w.columns > 0 && w.rows > 0)
                .then(|| FontSize::new(w.width / w.columns, w.height / w.rows))
        })
    };
    let explicit = match cfg.image_protocol.as_str() {
        "kitty" => Some(ProtocolType::Kitty),
        "sixel" => Some(ProtocolType::Sixel),
        "iterm2" => Some(ProtocolType::Iterm2),
        "halfblocks" => return Some(Picker::halfblocks()),
        _ => None,
    };
    if let Some(proto) = explicit {
        let mut p = font_from_ioctl().map(Picker::from_fontsize).unwrap_or_else(Picker::halfblocks);
        p.set_protocol_type(proto);
        return Some(p);
    }
    // The stdin capability query can leave a reader thread blocked on stdin when the
    // terminal does not answer every request (tmux, some multiplexers). That thread then
    // steals keystrokes, so only query when we are talking to the terminal directly.
    let in_mux = std::env::var_os("TMUX").is_some()
        || std::env::var("TERM").is_ok_and(|t| t.starts_with("screen"))
        || std::env::var_os("ZELLIJ").is_some();
    if in_mux {
        return Some(Picker::halfblocks());
    }
    // iTerm2 3.6+ and Warp answer the kitty graphics query, so ratatui-image would pick
    // kitty, whose Unicode placeholders neither terminal draws: the preview stays empty.
    // Konsole gets halfblocks from ratatui-image (its kitty support is partial and its
    // sixel leaves artifacts). All three implement the iTerm2 inline image protocol
    // (Konsole since 22.04), so skip the probes and use that.
    if wants_iterm2_protocol() {
        use ratatui_image::picker::cap_parser::QueryStdioOptions;
        let opts = QueryStdioOptions {
            blacklist_protocols: vec![ProtocolType::Kitty, ProtocolType::Sixel],
            ..Default::default()
        };
        let mut p = Picker::from_query_stdio_with_options(opts)
            .unwrap_or_else(|_| font_from_ioctl().map(Picker::from_fontsize).unwrap_or_else(Picker::halfblocks));
        p.set_protocol_type(ProtocolType::Iterm2);
        return Some(p);
    }
    Some(Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks()))
}

fn wants_iterm2_protocol() -> bool {
    std::env::var("TERM_PROGRAM").is_ok_and(|t| t.contains("iTerm") || t == "WarpTerminal")
        || std::env::var("LC_TERMINAL").is_ok_and(|t| t.contains("iTerm"))
        // KONSOLE_VERSION is YYMMPP, e.g. 230804 for 23.08.4.
        || std::env::var("KONSOLE_VERSION").ok().and_then(|v| v.parse::<u32>().ok()).is_some_and(|v| v >= 220400)
}

/// `pdeck --image-info`: which image protocol would be used here, and why.
fn print_image_info(cfg: &config::Config) {
    for k in ["TERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "LC_TERMINAL", "KONSOLE_VERSION", "TMUX", "ZELLIJ"] {
        println!("{k}={}", std::env::var(k).unwrap_or_default());
    }
    println!("image_protocol (config) = {}", cfg.image_protocol);
    let _ = terminal::enable_raw_mode();
    let raw = ratatui_image::picker::Picker::from_query_stdio();
    let picked = make_picker(cfg, false);
    let _ = terminal::disable_raw_mode();
    match raw {
        Ok(p) => println!("ratatui-image auto detection: {:?}, caps {:?}", p.protocol_type(), p.capabilities()),
        Err(e) => println!("ratatui-image auto detection failed: {e}"),
    }
    match picked {
        Some(p) => println!("pdeck uses: {:?}, font size {:?}", p.protocol_type(), p.font_size()),
        None => println!("pdeck uses: no images (preview_images = false)"),
    }
}

fn spawn_input_thread(tx: crossbeam_channel::Sender<AppEvent>, paused: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        loop {
            if paused.load(Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => {
                    if paused.load(Relaxed) {
                        continue;
                    }
                    match event::read() {
                        Ok(ev) => {
                            if tx.send(AppEvent::Term(ev)).is_err() {
                                return;
                            }
                        }
                        // Transient read errors (e.g. interrupted reads) must not kill input.
                        Err(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                }
                Ok(false) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    });
}

fn main() -> Result<()> {
    let args = parse_args();
    let (mut cfg, cfg_err) = config::Config::load();
    if args.image_info {
        print_image_info(&cfg);
        return Ok(());
    }
    if args.no_mouse {
        cfg.mouse = false;
    }
    // Restore the terminal on panic so the shell stays usable.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        pairdeck::vfs::dmg::detach_all();
        default_hook(info);
    }));

    pairdeck::svg::warm_up();
    let mut terminal = setup_terminal(cfg.mouse)?;
    let picker = make_picker(&cfg, args.no_images);
    let mut app = App::new(cfg, args.dirs.clone(), picker);
    if let Some(e) = cfg_err {
        app.error(format!("Config error, using defaults:\n{e}"));
    }
    let paused = Arc::new(AtomicBool::new(false));
    spawn_input_thread(app.tx.clone(), paused.clone());

    let res = run(&mut terminal, &mut app, &paused);
    app.save_state();
    // Never leave disk images attached.
    pairdeck::vfs::dmg::detach_all();
    restore_terminal();
    if let Some(f) = args.last_dir_file {
        let _ = std::fs::write(f, app.active_local_dir().display().to_string());
    }
    res
}

fn run(terminal: &mut Term, app: &mut App, paused: &Arc<AtomicBool>) -> Result<()> {
    let mut last_tick = std::time::Instant::now();
    loop {
        if app.full_redraw {
            terminal.clear()?;
            app.full_redraw = false;
        }
        terminal.draw(|f| ui::draw(f, app))?;
        // Animated previews need frames faster than the 100 ms UI tick.
        let wait = if app.previewer.animating() { Duration::from_millis(20) } else { Duration::from_millis(100) };
        match app.rx.recv_timeout(wait) {
            Ok(ev) => {
                app.handle_event(ev);
                // Drain queued events before the next redraw (fast typing, key repeat).
                for _ in 0..64 {
                    match app.rx.try_recv() {
                        Ok(ev) => app.handle_event(ev),
                        Err(_) => break,
                    }
                    if app.external.is_some() || app.quit {
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        app.previewer.tick();
        if last_tick.elapsed() >= Duration::from_millis(100) {
            app.tick();
            last_tick = std::time::Instant::now();
        }
        if let Some(ext) = app.external.take() {
            run_external(terminal, app, ext, paused)?;
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}

fn suspend(paused: &AtomicBool) {
    paused.store(true, Relaxed);
    std::thread::sleep(Duration::from_millis(70));
    restore_terminal();
}

fn resume(terminal: &mut Term, app: &App, paused: &AtomicBool) -> Result<()> {
    terminal::enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    if app.cfg.mouse {
        execute!(out, EnableMouseCapture)?;
    }
    if matches!(terminal::supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }
    terminal.clear()?;
    paused.store(false, Relaxed);
    Ok(())
}

fn wait_key() {
    print!("\r\n\x1b[7m Press any key to continue... \x1b[0m");
    let _ = stdout().flush();
    let _ = terminal::enable_raw_mode();
    loop {
        if let Ok(Event::Key(k)) = event::read()
            && k.kind == event::KeyEventKind::Press
        {
            break;
        }
    }
    let _ = terminal::disable_raw_mode();
    println!();
}

fn user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

fn run_external(terminal: &mut Term, app: &mut App, ext: External, paused: &Arc<AtomicBool>) -> Result<()> {
    suspend(paused);
    match ext {
        External::Command { cmd, cwd, pause } => {
            println!("\x1b[1m{}$ {}\x1b[0m", util::tildify(&cwd.display().to_string()), cmd);
            let status = std::process::Command::new(user_shell()).arg("-c").arg(&cmd).current_dir(&cwd).status();
            match status {
                Ok(s) if !s.success() => println!("\x1b[31m[exit status {}]\x1b[0m", s.code().unwrap_or(-1)),
                Err(e) => println!("\x1b[31m{e}\x1b[0m"),
                _ => {}
            }
            if pause {
                wait_key();
            }
        }
        External::Shell { cwd } => {
            let new_dir = run_subshell(&cwd);
            if let Some(d) = new_dir
                && d != cwd
                && d.is_dir()
                && app.panel().is_local()
            {
                let loc = vfs::Location::local(d);
                app.chdir_active(loc);
            }
        }
        External::Program { program, args, cwd } => {
            let status = std::process::Command::new(&program).args(&args).current_dir(&cwd).status();
            if let Err(e) = status {
                println!("{program}: {e}");
                wait_key();
            }
        }
        External::ShowOutput => wait_key(),
    }
    resume(terminal, app, paused)?;
    app.reload_panels();
    Ok(())
}

/// Run an interactive shell; return the directory it was in when it exited.
fn run_subshell(cwd: &Path) -> Option<PathBuf> {
    let shell = user_shell();
    let name = Path::new(&shell).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!("pairdeck-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let cwd_file = tmp.join("cwd");
    let _ = std::fs::remove_file(&cwd_file);
    let q = util::shell_quote(&cwd_file.display().to_string());
    println!("\x1b[2mpairdeck subshell. Type 'exit' or press Ctrl-D to return.\x1b[0m");
    let mut cmd = std::process::Command::new(&shell);
    cmd.current_dir(cwd).env("PAIRDECK_SUBSHELL", "1");
    match name.as_str() {
        "zsh" => {
            let orig = std::env::var("ZDOTDIR").unwrap_or_default();
            let zenv = "[ -n \"$PAIRDECK_ORIG_ZDOTDIR\" ] && ZDOTDIR=\"$PAIRDECK_ORIG_ZDOTDIR\" || ZDOTDIR=\"$HOME\"\n\
                        [ -f \"$ZDOTDIR/.zshenv\" ] && source \"$ZDOTDIR/.zshenv\"\n\
                        PAIRDECK_REAL_ZDOTDIR=\"$ZDOTDIR\"; ZDOTDIR=\"$PAIRDECK_TMP_ZDOTDIR\"\n";
            let zrc = format!(
                "ZDOTDIR=\"$PAIRDECK_REAL_ZDOTDIR\"\n[ -f \"$ZDOTDIR/.zshrc\" ] && source \"$ZDOTDIR/.zshrc\"\n\
                 zshexit() {{ pwd > {q} }}\n"
            );
            let _ = std::fs::write(tmp.join(".zshenv"), zenv);
            let _ = std::fs::write(tmp.join(".zshrc"), zrc);
            cmd.env("PAIRDECK_ORIG_ZDOTDIR", orig).env("PAIRDECK_TMP_ZDOTDIR", &tmp).env("ZDOTDIR", &tmp).arg("-i");
        }
        "bash" => {
            let rc = format!("[ -f ~/.bashrc ] && . ~/.bashrc\ntrap 'pwd > {q}' EXIT\n");
            let rcf = tmp.join("bashrc");
            let _ = std::fs::write(&rcf, rc);
            cmd.arg("--rcfile").arg(&rcf).arg("-i");
        }
        "fish" => {
            cmd.arg("-C").arg(format!("function __pairdeck_exit --on-event fish_exit; pwd > {q}; end"));
        }
        _ => {
            cmd.arg("-i");
        }
    }
    let _ = cmd.status();
    let dir = std::fs::read_to_string(&cwd_file).ok().map(|s| PathBuf::from(s.trim()));
    let _ = std::fs::remove_dir_all(&tmp);
    dir
}
