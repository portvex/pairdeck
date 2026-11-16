//! Command line interface: the option table, `--help`, shell completions
//! (`--completions bash|zsh|fish`) and the man page (`--man`). Everything is
//! generated from [`OPTIONS`] and the live default key map, so the outputs
//! cannot drift apart.

use crate::keymap::{Command, chord_label, default_bindings};
use std::collections::BTreeMap;

/// What an option's argument is, for completions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    None,
    File,
    Shell,
}

pub struct Opt {
    pub short: Option<char>,
    pub long: &'static str,
    pub arg: ArgKind,
    /// Placeholder shown in help, e.g. "FILE".
    pub value: &'static str,
    pub help: &'static str,
}

pub const OPTIONS: &[Opt] = &[
    Opt {
        short: Some('P'),
        long: "printwd",
        arg: ArgKind::File,
        value: "FILE",
        help: "write the last directory to FILE on exit (for cd-on-exit shell functions)",
    },
    Opt { short: Some('d'), long: "nomouse", arg: ArgKind::None, value: "", help: "disable mouse support" },
    Opt { short: None, long: "no-images", arg: ArgKind::None, value: "", help: "disable terminal graphics" },
    Opt {
        short: None,
        long: "image-info",
        arg: ArgKind::None,
        value: "",
        help: "show which terminal image protocol is used, then exit",
    },
    Opt { short: None, long: "sample-config", arg: ArgKind::None, value: "", help: "print a sample config file" },
    Opt {
        short: None,
        long: "completions",
        arg: ArgKind::Shell,
        value: "SHELL",
        help: "print a completion script for bash, zsh or fish",
    },
    Opt { short: None, long: "man", arg: ArgKind::None, value: "", help: "print the man page (roff)" },
    Opt { short: Some('V'), long: "version", arg: ArgKind::None, value: "", help: "print version" },
    Opt { short: Some('h'), long: "help", arg: ArgKind::None, value: "", help: "print this help" },
];

pub const SHELLS: &[&str] = &["bash", "zsh", "fish"];

const CD_ON_EXIT: &str = r#"pdcd() { local f=$(mktemp); pdeck -P "$f" "$@"; cd "$(cat "$f")"; rm -f "$f"; }"#;

fn flags(o: &Opt) -> String {
    let long = format!("--{}", o.long);
    let long = if o.value.is_empty() { long } else { format!("{long} {}", o.value) };
    match o.short {
        Some(s) => format!("-{s}, {long}"),
        None => format!("    {long}"),
    }
}

pub fn help_text() -> String {
    let mut s = format!(
        "pairdeck {} - modern twin-panel file manager\n\nUSAGE: pdeck [OPTIONS] [LEFT_DIR] [RIGHT_DIR]\n\nOPTIONS:\n",
        env!("CARGO_PKG_VERSION")
    );
    let w = OPTIONS.iter().map(|o| flags(o).len()).max().unwrap_or(0);
    for o in OPTIONS {
        s.push_str(&format!("  {:<w$}  {}\n", flags(o), o.help));
    }
    s.push_str("\nShell integration (cd to the last directory on exit):\n  ");
    s.push_str(CD_ON_EXIT);
    s.push_str("\n\nRun `pdeck --man | man -l -` (Linux) or `man ./pdeck.1` for the full manual.");
    s
}

// ---------------------------------------------------------------- completions

pub fn completions(shell: &str) -> Option<String> {
    match shell {
        "bash" => Some(bash()),
        "zsh" => Some(zsh()),
        "fish" => Some(fish()),
        _ => None,
    }
}

fn all_flags() -> Vec<String> {
    let mut v = Vec::new();
    for o in OPTIONS {
        if let Some(s) = o.short {
            v.push(format!("-{s}"));
        }
        v.push(format!("--{}", o.long));
    }
    v
}

fn flags_with(kind: ArgKind) -> String {
    let mut v = Vec::new();
    for o in OPTIONS.iter().filter(|o| o.arg == kind) {
        if let Some(s) = o.short {
            v.push(format!("-{s}"));
        }
        v.push(format!("--{}", o.long));
    }
    v.join("|")
}

fn bash() -> String {
    format!(
        r#"# bash completion for pdeck (pairdeck). Install: pdeck --completions bash > ~/.local/share/bash-completion/completions/pdeck
_pdeck() {{
    local cur prev
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    case "$prev" in
        {file})
            COMPREPLY=($(compgen -f -- "$cur")); return ;;
        {shell})
            COMPREPLY=($(compgen -W "{shells}" -- "$cur")); return ;;
    esac
    if [[ "$cur" == -* ]]; then
        COMPREPLY=($(compgen -W "{all}" -- "$cur"))
    else
        COMPREPLY=($(compgen -d -- "$cur"))
    fi
}}
complete -o filenames -F _pdeck pdeck
"#,
        file = flags_with(ArgKind::File),
        shell = flags_with(ArgKind::Shell),
        shells = SHELLS.join(" "),
        all = all_flags().join(" "),
    )
}

fn zsh_escape(s: &str) -> String {
    s.replace('\'', "'\\''").replace('[', "\\[").replace(']', "\\]").replace(':', "\\:")
}

fn zsh() -> String {
    let mut specs = Vec::new();
    for o in OPTIONS {
        let action = match o.arg {
            ArgKind::None => String::new(),
            ArgKind::File => format!(":{}:_files", o.value.to_lowercase()),
            ArgKind::Shell => format!(":shell:({})", SHELLS.join(" ")),
        };
        let help = zsh_escape(o.help);
        match o.short {
            Some(s) => specs.push(format!("    '(-{s} --{l})'{{-{s},--{l}}}'[{help}]{action}'", l = o.long)),
            None => specs.push(format!("    '--{l}[{help}]{action}'", l = o.long)),
        }
    }
    format!(
        "#compdef pdeck\n# zsh completion for pdeck (pairdeck). Install: pdeck --completions zsh > \"${{fpath[1]}}/_pdeck\"\n\n_arguments -s \\\n{} \\\n    '1:left directory:_directories' \\\n    '2:right directory:_directories'\n",
        specs.join(" \\\n")
    )
}

fn fish() -> String {
    let mut s = String::from(
        "# fish completion for pdeck (pairdeck). Install: pdeck --completions fish > ~/.config/fish/completions/pdeck.fish\n\
         complete -c pdeck -f -a '(__fish_complete_directories)'\n",
    );
    for o in OPTIONS {
        let mut line = String::from("complete -c pdeck");
        if let Some(c) = o.short {
            line.push_str(&format!(" -s {c}"));
        }
        line.push_str(&format!(" -l {}", o.long));
        match o.arg {
            ArgKind::None => {}
            ArgKind::File => line.push_str(" -r -F"),
            ArgKind::Shell => line.push_str(&format!(" -x -a '{}'", SHELLS.join(" "))),
        }
        line.push_str(&format!(" -d '{}'", o.help.replace('\'', "\\'")));
        s.push_str(&line);
        s.push('\n');
    }
    s
}

// ---------------------------------------------------------------- man page

/// Escape text for roff: backslashes, leading dots / quotes, hyphens in flags.
fn roff(s: &str) -> String {
    let s = s.replace('\\', "\\e").replace('-', "\\-");
    if s.starts_with('.') || s.starts_with('\'') { format!("\\&{s}") } else { s }
}

pub fn man_page() -> String {
    let mut m = String::new();
    // Date of the last manual revision (shown in the page footer).
    let date = "2026-11-16";
    m.push_str(&format!(".TH PDECK 1 \"{date}\" \"pairdeck {}\" \"User Commands\"\n", env!("CARGO_PKG_VERSION")));
    m.push_str(".SH NAME\nrc \\- pairdeck, a modern twin\\-panel terminal file manager\n");
    m.push_str(".SH SYNOPSIS\n.B pdeck\n[\\fIOPTIONS\\fR] [\\fILEFT_DIR\\fR] [\\fIRIGHT_DIR\\fR]\n");
    m.push_str(
        ".SH DESCRIPTION\n\
         .B pdeck\nis a twin\\-panel file manager in the spirit of Midnight Commander: two\n\
         directory panels, F1\\-F10 commands, a shell command line and Ctrl\\-X chords,\n\
         plus image and syntax\\-highlighted previews, Nerd Font icons, themes, mouse\n\
         support, tabs, git status, a fuzzy finder and an embedded terminal.\n\
         Archives (zip, tar, 7z, ISO, DMG), SFTP, S3 and WebDAV locations are browsed\n\
         like directories.\n\
         .PP\nThe active panel has the highlighted border; Tab switches panels. Typing\n\
         writes on the command line and Enter runs it in the active directory. With an\n\
         empty command line Enter opens directories, archives and files. Press F1 for\n\
         the built\\-in help.\n",
    );
    m.push_str(".SH OPTIONS\n");
    for o in OPTIONS {
        let mut head = String::new();
        if let Some(s) = o.short {
            head.push_str(&format!("\\fB\\-{s}\\fR, "));
        }
        head.push_str(&format!("\\fB\\-\\-{}\\fR", roff(o.long)));
        if !o.value.is_empty() {
            head.push_str(&format!(" \\fI{}\\fR", o.value));
        }
        m.push_str(&format!(".TP\n{head}\n{}\n", roff(o.help)));
    }

    // Key bindings, generated from the default key map: one entry per command,
    // listing all of its keys, sorted by command name.
    m.push_str(".SH KEY BINDINGS\nDefault bindings; change them in the \\fB[keys]\\fR table of the config file\nor with Options > Key bindings.\n");
    let mut by_cmd: BTreeMap<&'static str, (Command, Vec<String>)> = BTreeMap::new();
    for (chord, cmd) in default_bindings() {
        by_cmd.entry(cmd.name()).or_insert((cmd, Vec::new())).1.push(chord_label(&chord));
    }
    for (_, (cmd, mut keys)) in by_cmd {
        keys.sort_by_key(|k| (k.len(), k.clone()));
        m.push_str(&format!(
            ".TP\n\\fB{}\\fR\n{} (\\fI{}\\fR)\n",
            roff(&keys.join(", ")),
            roff(cmd.desc()),
            roff(cmd.name())
        ));
    }
    m.push_str(
        ".PP\nKey notation: C\\- is Ctrl, M\\- is Alt (Option on macOS), S\\- is Shift;\n\
         a space separates the keys of a chord (C\\-x d means Ctrl\\-X, then d).\n",
    );

    m.push_str(&format!(
        ".SH SHELL INTEGRATION\nTo change to pairdeck's last directory when it exits, add to your shell rc file:\n.PP\n.nf\n{}\n.fi\n\
         .PP\nCompletion scripts: \\fBrc \\-\\-completions bash|zsh|fish\\fR.\n",
        roff(CD_ON_EXIT)
    ));
    m.push_str(
        ".SH FILES\n\
         .TP\n\\fI~/.config/pairdeck/config.toml\\fR\nSettings and the \\fB[keys]\\fR table (\\fBrc \\-\\-sample\\-config\\fR prints an example).\n\
         .TP\n\\fI~/.config/pairdeck/themes/NAME.toml\\fR\nCustom themes.\n\
         .TP\n\\fI~/.config/pairdeck/menu\\fR, \\fI.pairdeck.menu\\fR\nUser menu (F2), Midnight Commander format; a local \\fI.pairdeck.menu\\fR wins.\n\
         .TP\n\\fI~/.config/pairdeck/assoc.toml\\fR\nFile associations (Enter, F3, F4).\n\
         .TP\n\\fI~/.local/share/pairdeck/state.toml\\fR\nHotlist, histories, tabs and frecent directories.\n",
    );
    m.push_str(
        ".SH ENVIRONMENT\n\
         .TP\n\\fBPAIRDECK_CONFIG_DIR\\fR, \\fBPAIRDECK_DATA_DIR\\fR\nOverride the config and data directories.\n\
         .TP\n\\fBXDG_CONFIG_HOME\\fR, \\fBXDG_DATA_HOME\\fR\nBase directories when the variables above are unset.\n\
         .TP\n\\fBSHELL\\fR\nShell for the command line, Ctrl\\-O subshell and the terminal pane.\n\
         .TP\n\\fBVISUAL\\fR, \\fBEDITOR\\fR, \\fBPAGER\\fR\nUsed when the internal editor or viewer is turned off.\n\
         .TP\n\\fBAWS_ACCESS_KEY_ID\\fR, \\fBAWS_SECRET_ACCESS_KEY\\fR, \\fBAWS_PROFILE\\fR, \\fBAWS_REGION\\fR, \\fBAWS_ENDPOINT_URL\\fR\nCredentials and endpoint for s3:// locations.\n",
    );
    m.push_str(".SH EXIT STATUS\n0 on a normal exit, 1 on a fatal error.\n");
    m.push_str(".SH SEE ALSO\n.BR mc (1),\n.BR yazi (1)\n");
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_option_appears_everywhere() {
        let help = help_text();
        let man = man_page();
        for shell in SHELLS {
            let script = completions(shell).unwrap();
            for o in OPTIONS {
                assert!(script.contains(o.long), "{shell} completion lacks --{}", o.long);
            }
        }
        for o in OPTIONS {
            assert!(help.contains(&format!("--{}", o.long)), "help lacks --{}", o.long);
            assert!(man.contains(&roff(o.long)), "man lacks --{}", o.long);
        }
        assert!(completions("powershell").is_none());
    }

    #[test]
    fn man_page_lists_every_bound_command() {
        let man = man_page();
        let mut missing: Vec<&str> = Vec::new();
        for (_, cmd) in default_bindings() {
            if !man.contains(&format!("(\\fI{}\\fR)", roff(cmd.name()))) {
                missing.push(cmd.name());
            }
        }
        assert!(missing.is_empty(), "man page lacks {missing:?}");
        assert!(man.starts_with(".TH PDECK 1"));
        // Every line starting with a dot must be one of the requests we emit
        // (text lines that start with a dot are escaped by `roff`).
        const REQUESTS: &[&str] = &[".TH", ".SH", ".TP", ".PP", ".B", ".BR", ".nf", ".fi"];
        for l in man.lines().filter(|l| l.starts_with('.')) {
            let req = l.split_whitespace().next().unwrap_or("");
            assert!(REQUESTS.contains(&req), "unexpected roff line: {l}");
        }
    }
}
