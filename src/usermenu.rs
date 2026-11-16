//! F2 user menu, compatible with the Midnight Commander menu file format.
//!
//! Format: an entry starts at column 0 with a hotkey, whitespace and a title; the
//! following indented lines are the shell script. Lines starting with `+` (show if)
//! or `=` (default if) before an entry are conditions. `#` starts a comment.

use crate::app::{App, External};
use crate::dialog::{ListAction, ListDialog, ListItem};
use crate::util::shell_quote;
use crate::vfs::{Entry, Location};
use std::path::{Path, PathBuf};

pub const DEFAULT_MENU: &str = r#"# pairdeck user menu (MC compatible). Edit with Command > Edit user menu file.
# Macros: %f file  %d dir  %F/%D other panel file/dir  %s selection  %T other selection
#         %u selection (then unmark)  %b basename without extension  %x extension
#         %{Prompt text} asks for a value  %% literal percent

+ t r | t d
c       Compress selection to tar.gz
	tar -czvf %{Archive name (without .tar.gz)}.tar.gz %s

+ t r | t d
z       Compress selection to zip
	zip -r %{Archive name (without .zip)}.zip %s

+ f *.tar.gz | f *.tgz | f *.tar.bz2 | f *.tar.xz | f *.tar | f *.tbz2 | f *.txz | f *.tar.zst
x       Extract tar archive here
	tar -xvf %f

+ f *.zip | f *.jar | f *.apk | f *.whl
x       Extract zip archive here
	unzip %f

+ f *.7z
x       Extract 7z archive here
	7z x %f

+ f *.rar
x       Extract rar archive here
	unrar x %f || 7z x %f

g       Git status
	git status

l       Git log (last 30)
	git log --oneline --graph --decorate -n 30

D       Git diff
	git --no-pager diff --stat; git --no-pager diff

e       Open in $EDITOR
	${EDITOR:-vi} %f

u       Disk usage of selection
	du -sh %s

+ t r
X       Make executable
	chmod +x %s && ls -l %s

h       SHA-256 checksums of selection
	shasum -a 256 %s 2>/dev/null || sha256sum %s

+ f *.png | f *.jpg | f *.jpeg | f *.gif | f *.webp | f *.tiff | f *.heic | f *.bmp
i       Image info
	if command -v sips >/dev/null; then sips -g all %f; else identify -verbose %f | head -40; fi

w       Word count of selection
	wc %s

+ t r
v       Copy file to other panel with a new name
	cp -i %f %D/%{New name}

r       Run command on each selected file
	CMD=%{Command (file appended)}
	for f in %s; do echo "== $f"; $CMD "$f"; done
"#;

/// One user menu entry.
pub struct MenuEntry {
    pub hotkey: String,
    pub title: String,
    pub script: String,
    pub show: Option<String>,
}

fn menu_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("menu"))
}

/// Parse an MC-format user menu file (public for fuzzing).
pub fn parse(text: &str) -> Vec<MenuEntry> {
    let mut out: Vec<MenuEntry> = Vec::new();
    let mut pending_cond: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = out.last_mut() {
                let l = line.strip_prefix('\t').unwrap_or_else(|| line.trim_start());
                last.script.push_str(l);
                last.script.push('\n');
            }
            continue;
        }
        // Condition line: "+ cond", "= cond", "+= cond", "=+ cond".
        let mut chars = line.chars();
        let c0 = chars.next().unwrap_or(' ');
        let c1 = chars.next().unwrap_or(' ');
        if (c0 == '+' || c0 == '=') && (c1.is_whitespace() || ((c1 == '+' || c1 == '=') && c1 != c0)) {
            let rest = line.trim_start_matches(['+', '=']).trim();
            if line.starts_with('+') || line[1..].starts_with('+') {
                pending_cond = Some(rest.to_string());
            }
            continue;
        }
        let hotkey: String = line.chars().take(1).collect();
        let title = line[hotkey.len()..].trim().to_string();
        out.push(MenuEntry { hotkey, title, script: String::new(), show: pending_cond.take() });
    }
    out
}

struct CondCtx<'a> {
    file: Option<&'a Entry>,
    dir: String,
}

fn glob_match(pat: &str, s: &str) -> bool {
    crate::util::glob_matcher(pat, true).is_some_and(|m| m.is_match(s))
}

fn eval_atom(atom: &str, c: &CondCtx) -> bool {
    let mut atom = atom.trim();
    let mut neg = false;
    while let Some(r) = atom.strip_prefix('!') {
        neg = !neg;
        atom = r.trim_start();
    }
    let mut parts = atom.splitn(2, char::is_whitespace);
    let kind = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    let v = match kind {
        "f" => c.file.is_some_and(|e| arg.split_whitespace().any(|p| glob_match(p, &e.name))),
        "d" => arg.split_whitespace().any(|p| {
            glob_match(p, &c.dir)
                || glob_match(
                    p,
                    Path::new(&c.dir).file_name().map(|s| s.to_string_lossy()).unwrap_or_default().as_ref(),
                )
        }),
        "t" => arg.chars().any(|t| match (t, c.file) {
            ('d', Some(e)) => e.is_dir() && !e.is_parent(),
            ('r', Some(e)) => !e.is_dir() && !e.is_symlink(),
            ('x', Some(e)) => e.is_exec(),
            ('l', Some(e)) => e.is_symlink(),
            ('f', Some(e)) => !e.is_dir(),
            (c, _) if c.is_ascii_uppercase() => true,
            _ => false,
        }),
        "x" => c.file.is_some_and(|e| e.is_exec()),
        _ => true,
    };
    v != neg
}

/// Evaluate "a | b & c" left to right (MC semantics: no precedence).
fn eval(cond: &str, c: &CondCtx) -> bool {
    let mut result: Option<bool> = None;
    let mut op = '|';
    let mut cur = String::new();
    let flush = |cur: &mut String, op: char, result: &mut Option<bool>| {
        if cur.trim().is_empty() {
            return;
        }
        let v = eval_atom(cur, c);
        *result = Some(match (result.take(), op) {
            (None, _) => v,
            (Some(r), '&') => r && v,
            (Some(r), _) => r || v,
        });
        cur.clear();
    };
    for ch in cond.chars() {
        if ch == '|' || ch == '&' {
            flush(&mut cur, op, &mut result);
            op = ch;
        } else {
            cur.push(ch);
        }
    }
    flush(&mut cur, op, &mut result);
    result.unwrap_or(true)
}

fn quoted_list(names: &[String]) -> String {
    names.iter().map(|n| shell_quote(n)).collect::<Vec<_>>().join(" ")
}

/// Expand macros. `%{...}` prompts are replaced by `\0N\0` placeholders, returned in order.
fn expand(app: &App, script: &str, unmark: &mut bool) -> (String, Vec<String>) {
    let p = app.panel();
    let o = app.other();
    let cur = p.current().filter(|e| !e.is_parent()).map(|e| e.name.clone()).unwrap_or_default();
    let ocur = o.current().filter(|e| !e.is_parent()).map(|e| e.name.clone()).unwrap_or_default();
    let sel: Vec<String> = p.selection().into_iter().map(|e| e.name).collect();
    let osel: Vec<String> = o.selection().into_iter().map(|e| e.name).collect();
    let (stem, ext) = match cur.rfind('.') {
        Some(i) if i > 0 => (cur[..i].to_string(), cur[i + 1..].to_string()),
        _ => (cur.clone(), String::new()),
    };
    let mut out = String::new();
    let mut prompts = Vec::new();
    let mut it = script.chars().peekable();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('%') => out.push('%'),
            Some('f') | Some('p') => out.push_str(&shell_quote(&cur)),
            Some('F') => out.push_str(&shell_quote(&o.loc.join(&ocur).display().to_string())),
            Some('d') => out.push_str(&shell_quote(&p.loc.path.display().to_string())),
            Some('D') => out.push_str(&shell_quote(&o.loc.path.display().to_string())),
            Some('s') | Some('t') => out.push_str(&quoted_list(&sel)),
            Some('T') => out.push_str(&quoted_list(&osel)),
            Some('u') => {
                *unmark = true;
                out.push_str(&quoted_list(&sel));
            }
            Some('U') => out.push_str(&quoted_list(&osel)),
            Some('x') => out.push_str(&shell_quote(&ext)),
            Some('b') | Some('n') => out.push_str(&shell_quote(&stem)),
            Some('{') => {
                let mut text = String::new();
                for ch in it.by_ref() {
                    if ch == '}' {
                        break;
                    }
                    text.push(ch);
                }
                out.push_str(&format!("\0{}\0", prompts.len()));
                prompts.push(text);
            }
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    (out, prompts)
}

fn ask_prompts(
    app: &mut App,
    script: String,
    prompts: Vec<String>,
    mut answers: Vec<String>,
    cwd: PathBuf,
    unmark: bool,
) {
    if answers.len() >= prompts.len() {
        let mut cmd = script;
        for (i, a) in answers.iter().enumerate() {
            cmd = cmd.replace(&format!("\0{i}\0"), &shell_quote(a));
        }
        if unmark {
            app.panel_mut().marked.clear();
        }
        app.external = Some(External::Command { cmd: cmd.trim_end().to_string(), cwd, pause: true });
        return;
    }
    let label = prompts[answers.len()].clone();
    let d = crate::dialog::input(app, "User menu", &label, "", "usermenu", move |app, v| {
        answers.push(v);
        ask_prompts(app, script, prompts, answers, cwd, unmark);
    });
    app.push_dialog(d);
}

fn load_text(app: &App) -> (String, String) {
    if let Some(dir) = app.panel().loc.local_path() {
        let local = dir.join(".pairdeck.menu");
        if let Ok(t) = std::fs::read_to_string(&local) {
            return (t, ".pairdeck.menu".into());
        }
    }
    if let Some(p) = menu_path()
        && let Ok(t) = std::fs::read_to_string(&p)
    {
        return (t, "User menu".into());
    }
    (DEFAULT_MENU.to_string(), "User menu".into())
}

pub fn open(app: &mut App) {
    let (text, title) = load_text(app);
    let entries = parse(&text);
    let cur = app.panel().current().cloned();
    let ctx = CondCtx { file: cur.as_ref(), dir: app.panel().loc.path.display().to_string() };
    let visible: Vec<MenuEntry> =
        entries.into_iter().filter(|e| e.show.as_deref().is_none_or(|c| eval(c, &ctx))).collect();
    if visible.is_empty() {
        app.info("No user menu entries apply here");
        return;
    }
    let items: Vec<ListItem> = visible.iter().map(|e| ListItem::new(&e.title).hint(&e.hotkey)).collect();
    let scripts: Vec<String> = visible.into_iter().map(|e| e.script).collect();
    let mut d = ListDialog::new(&title, items, move |app, a| {
        if let ListAction::Select(i) = a {
            let mut unmark = false;
            let (script, prompts) = expand(app, &scripts[i], &mut unmark);
            let cwd = app.active_local_dir();
            ask_prompts(app, script, prompts, Vec::new(), cwd, unmark);
        }
    });
    d.filterable = false;
    app.push_dialog(Box::new(d.footer("Press the hotkey or Enter")));
}

pub fn edit_menu_file(app: &mut App) {
    let Some(path) = menu_path() else {
        app.error("No configuration directory");
        return;
    };
    let dir = path.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    if !path.exists() {
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = std::fs::write(&path, DEFAULT_MENU) {
            app.error(format!("Cannot write {}:\n{e}", path.display()));
            return;
        }
    }
    app.open_editor(&Location::local(dir), &path);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_default() {
        let e = parse(DEFAULT_MENU);
        assert!(e.len() > 10);
        assert_eq!(e[0].hotkey, "c");
        assert!(e[0].script.contains("tar -czvf"));
        assert!(e[0].show.is_some());
    }
    #[test]
    fn conditions() {
        let f = Entry::new("a.tar.gz", crate::vfs::EntryKind::File);
        let c = CondCtx { file: Some(&f), dir: "/tmp".into() };
        assert!(eval("f *.zip | f *.tar.gz", &c));
        assert!(!eval("f *.zip", &c));
        assert!(eval("t r", &c));
        assert!(!eval("t d", &c));
        assert!(eval("! t d", &c));
    }
}
