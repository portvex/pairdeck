# pairdeck

A modern twin-panel terminal file manager written in Rust, in the spirit of
Midnight Commander: the same keyboard muscle memory (F1-F10, Ctrl-O, Ctrl-X
chords, Insert to mark), plus image previews, Nerd Font icons, themes, mouse
support and archive/SFTP browsing.

Inspired by [Midnight Commander](https://midnight-commander.org/), the classic
twin-panel file manager by Miguel de Icaza and the GNU Midnight Commander team.
pairdeck is an independent project written from scratch in Rust and is not
affiliated with it.

Binary name: `pdeck`

![pairdeck: two panels with Nerd Font icons](docs/screenshots/panels.png)

## Screenshots

| | |
|---|---|
| ![Quick view with syntax highlighting](docs/screenshots/preview-code.png) | ![Image preview](docs/screenshots/preview-image.png) |
| **Quick view** (Ctrl-Q): syntax-highlighted source of the file under the cursor | **Image preview**: real pixels in Kitty, Ghostty, iTerm2, WezTerm, Warp and Konsole (shown here as colored blocks, the fallback) |
| ![Fuzzy finder](docs/screenshots/fuzzy.png) | ![Classic theme and copy dialog](docs/screenshots/classic.png) |
| **Fuzzy finder** (Ctrl-P) over the whole tree, with a live preview | **`classic` theme**: the familiar blue Midnight Commander look, here with the F5 copy dialog |

## Install

| Where | Command |
|---|---|
| Homebrew (macOS, Linux) | `brew install portvex/tap/pairdeck` |
| Debian / Ubuntu | download `pairdeck_<version>_amd64.deb` or `_arm64.deb` from the [latest release](https://github.com/portvex/pairdeck/releases/latest), then `sudo apt install ./pairdeck_*.deb` |
| Cargo | `cargo install pairdeck` (Rust 1.89+ and a C compiler) |
| Prebuilt binary | static Linux (x86_64, aarch64) and universal macOS tarballs on the [releases page](https://github.com/portvex/pairdeck/releases) |

Every release file has a signed build provenance attestation:
`gh attestation verify pairdeck-<version>-x86_64-unknown-linux-musl.tar.gz -R portvex/pairdeck`.

## Build

```sh
cargo build --release
./target/release/pdeck [LEFT_DIR] [RIGHT_DIR]
```

Requirements: Rust 1.89+ (edition 2024). No system libraries are needed: SFTP
uses russh (pure Rust SSH) and HTTPS uses rustls, so there is no OpenSSL.

For a self-contained binary to copy to other machines, use the `static`
profile. Every C library (liblzma, zstd and the crypto core of ring) is compiled in;
on Linux, a musl target makes the executable fully static:

```sh
cargo build --profile static --target x86_64-unknown-linux-musl   # Linux, no dependencies at all
cargo build --profile static                                      # macOS: only system libraries
scripts/check-static.sh target/x86_64-unknown-linux-musl/static/pdeck
```

From macOS, `cargo zigbuild --profile static --target x86_64-unknown-linux-musl`
([cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)) cross-builds
the Linux binary. Windows is not supported yet. A [Nerd Font](https://www.nerdfonts.com/) in your
terminal is recommended for icons (`nerd_fonts = false` disables them).

### Man page and shell completions

```sh
pdeck --man > pdeck.1 && man ./pdeck.1                       # or: pdeck --man | man -l -   (Linux)
pdeck --completions bash > ~/.local/share/bash-completion/completions/pdeck
pdeck --completions zsh  > "${fpath[1]}/_pdeck"            # any directory in $fpath
pdeck --completions fish > ~/.config/fish/completions/pdeck.fish
```

Completion covers the options, shell names after `--completions`, files after
`-P` and directories for the two panel arguments. The man page's key binding
section is generated from the default key map, so it is always current. The
Homebrew formula installs both automatically; release tarballs include them.

### Shell integration

Change to the last directory when you quit:

```sh
pdcd() { local f=$(mktemp); pdeck -P "$f" "$@"; cd "$(cat "$f")"; rm -f "$f"; }
```

## Features

| Area | What you get |
|------|--------------|
| Panels | Full / brief / long listings, info panel, directory tree, quick view; sort by name, natural, extension, size, times, inode; filters; hidden files toggle; directory sizes (Ctrl-Space); auto refresh on file system changes |
| Selection | Insert / Ctrl-T, `+` `-` `*` glob select/unselect/invert, mouse right click |
| File operations | Copy, move, rename, mkdir (nested), delete or move to trash; run in background with progress, pause, abort; overwrite / skip / older / rename on conflicts; MC-style target masks (`*.bak`) |
| Preview | Images (including SVG/SVGZ) through the Kitty, iTerm2 or Sixel protocols (halfblocks fallback), syntax-highlighted text (about 210 languages, bat's grammar set), Markdown, CSV/TSV tables, pretty JSON with a collapsible tree, TOML validation, photo EXIF, font sample sheets, animated GIF/APNG/WebP, ELF/Mach-O/PE summaries, docx/odt/pptx/xlsx/epub text, colored plain text and logs, directories, archive contents, PDF first page, video thumbnails, audio metadata, hex dump |
| Viewer (F3) | Text with syntax highlighting, wrap, hex mode, regex search, goto; full-screen images and SVG with zoom and pan (`t` shows the SVG source) |
| Editor (F4) | Syntax highlighting, undo/redo, selection, clipboard (OSC 52), search and replace, auto indent |
| VFS | Browse zip (including password-protected ZipCrypto/AES), tar, tar.gz/bz2/xz/zst, gz/bz2/xz/zst, 7z archives, ISO 9660 images (Joliet, Rock Ridge) and macOS disk images (.dmg, mounted read-only) like directories, and write into local zip / tar* / 7z archives (copy in, mkdir, delete, chmod, symlinks); remote file systems: SFTP (ssh-agent, keys or password), S3 and S3-compatible storage (MinIO, R2, Ceph...), WebDAV (Nextcloud, ownCloud...) |
| Images | Rotate, flip, resize (fit, exact, percent, crop-to-fill), convert (PNG, JPEG, WebP, BMP, TIFF, GIF, QOI, ICO; SVG input), strip metadata, auto-orient; lossless JPEG rotation; background job with undo |
| Tools | Directory sync (Ctrl-X Y: recursive, one-way / two-way / mirror, editable plan), fuzzy finder with preview (Ctrl-P), zoxide-style directory jumps (Alt-J, `z`), find file (name glob + content regex, results to panel), compare directories, side-by-side diff, chmod, chown, symlinks / hard links, user menu (F2) with MC macros, file associations, external panelize, hotlist, directory and command history |
| Shell | Command line with Tab completion, embedded terminal as a bottom pane (Alt-Z) or as one of the panels (Ctrl-X Ctrl-Z), Ctrl-O subshell that syncs its final directory back, Ctrl-X Ctrl-O to see previous output |
| Look | Themes: `dark`, `classic` (MC blue), `nord`, `gruvbox`, `light`, plus your own TOML themes; rebindable keys; mouse |

## Configuration

| File | Purpose |
|------|---------|
| `~/.config/pairdeck/config.toml` | Settings and `[keys]` rebinding (`pdeck --sample-config`) |
| `~/.config/pairdeck/themes/<name>.toml` | Custom themes |
| `~/.config/pairdeck/menu` | User menu (F2), MC format |
| `~/.config/pairdeck/assoc.toml` | File associations |
| `~/.local/share/pairdeck/state.toml` | Hotlist and histories |

Changes to these files apply immediately while pairdeck runs (live reload,
`live_reload = false` turns it off). A file with errors is reported in a
dialog and the previous settings stay in effect.

**Options > Key bindings...** edits bindings interactively: type to filter the
command list, Enter and then press a key or chord to bind it (you are asked
before taking a key from another command), Del removes a command's keys, Ctrl-R
resets everything. Changes are saved to the `[keys]` table of config.toml; the
rest of the file, comments included, is left as it was.

Key binding example:

```toml
[keys]
"ctrl-p" = "find_file"
"ctrl-x d" = "compare_dirs"
"F12" = "theme_cycle"
"alt-t" = "none"          # unbind
```

Theme example (`~/.config/pairdeck/themes/mine.toml`, then `theme = "mine"`). `syntax` accepts any bundled highlighting theme: Catppuccin
Mocha/Macchiato/Frappe/Latte, Nord, gruvbox-dark/light, Dracula, GitHub,
OneHalfDark/Light, Solarized (dark)/(light), Monokai Extended, TwoDark, zenburn and more):

```toml
base = "dark"
syntax = "Solarized (dark)"
[styles]
cursor = "black on #88c0d0 bold"
dir = "#81a1c1 bold"
```

## Preview pane

Ctrl-Q (also Alt-Q, Ctrl-X q, or click **Preview** on the menu bar) turns the
other panel into a live preview of the file under the cursor. Images are drawn
with real pixels in Ghostty, Kitty, WezTerm, iTerm2, Warp and Konsole (22.04+), with Sixel where
supported, and with colored blocks inside tmux. The preview stays open across
restarts until you close it ([x] or the same key).

If images do not show up, `pdeck --image-info` prints what the terminal
reported and which protocol pdeck chose; `image_protocol` in `config.toml`
(`kitty`, `sixel`, `iterm2`, `halfblocks`) overrides the automatic choice, for
example over SSH where the terminal's environment variables are not forwarded.

Images in the preview can be zoomed up to 32x:

| Action | Mouse | Keys (file list) | Keys (preview focused, Tab) |
|--------|-------|------------------|-----------------------------|
| Zoom in / out | wheel over the image (zooms toward the pointer) | Alt-= / Alt-- | + / - |
| Pan | drag | Alt-arrows | arrows or h j k l |
| Fit | double-click (double-click again for 2x) | Alt-0 | 0 or Home |

Zooming reuses the decoded image (PDF pages and video frames too), so it does
not re-read the file; strong magnification switches to crisp nearest-neighbor
scaling. The F3 viewer zooms images and SVGs the same way (wheel toward the
pointer, drag, double-click, `+ - 0`, arrows or `h j k l` when zoomed).

### What the preview understands

| File | Preview | Keys (preview focused) |
|------|---------|------------------------|
| CSV, TSV, PSV | Aligned table, delimiter detected, numbers right-aligned | Left / Right scroll |
| JSON, JSON Lines | Pretty printed and highlighted; collapsible tree | `t` tree, arrows, Enter |
| TOML | Highlighted; parse errors shown on top | |
| JPEG / TIFF / HEIF photos | EXIF summary (camera, lens, exposure, ISO, GPS), orientation applied | zoom keys |
| TTF, OTF, TTC | Sample sheet at several sizes, family, style, glyph count | zoom keys |
| GIF, APNG, animated WebP | Plays the animation (zoom is off while it plays) | Space pause |
| ELF, Mach-O (universal too), PE, `.a` | Format, architecture, type, entry, libraries, sections | |
| docx, odt, ods, odp, pptx, xlsx, epub | Extracted text | |
| Text and logs without a grammar | URLs, e-mails, dates, numbers, paths and log levels colored | Left / Right scroll |

## Git

Inside a git repository every file gets a status marker (modified, staged,
added, untracked, deleted, renamed, conflict, ignored) and directories show
the strongest status below them. The panel border shows the branch (or the
detached commit), ahead/behind counts and `*` when the tree is dirty. Status
comes from `git status --porcelain=v2` on a background thread with a 3 s
timeout, so large repositories never freeze the UI.

The quick view shows `git diff HEAD` for modified files (press `d` in the
focused preview to see the file instead; `preview_git_diff = false` turns it off).

| Keys | Action (on marked files or the file under the cursor) |
|------|------------------------------------------------------|
| Ctrl-G s | stage |
| Ctrl-G u | unstage |
| Ctrl-G r | discard changes (asks first) |
| Ctrl-G d | side-by-side diff against HEAD |
| Ctrl-G l | log of the file (follows renames) or directory |
| Ctrl-G b | blame |
| Ctrl-G c | commit dialog (message, -a, amend) |
| Ctrl-G g | full `git status` |

The same commands are in the Command menu. `git_status = false` disables the
integration. Theme files can restyle `git_modified`, `git_staged`, `git_added`,
`git_untracked`, `git_deleted`, `git_renamed`, `git_conflict`, `git_ignored`
and `git_branch`.

## Fuzzy finder and directory jumps

**Ctrl-P** opens a fuzzy finder over every file below the active directory,
with a live preview of the selected file. It walks the tree in the background
(results stream in), respects `.gitignore` / `.ignore`, skips `.git`, and uses
the same scoring as the Helix editor (fzf syntax: `foo bar` both, `!test`
exclude, `^src` prefix, `.rs$` suffix, `'exact`).

| Key | Action |
|-----|--------|
| Enter | Go to the file in the active panel (or into a directory) |
| F3 / F4 | View / edit |
| Ctrl-O | Open its directory in the other panel |
| Alt-Enter | Put the path on the command line |
| Tab, then F2 or Ctrl-Space | Mark results, then show them as a panel (panelize) |
| Alt-H / Alt-I / Alt-D | Hidden files / gitignore on-off / directories only |
| Alt-V | Toggle the preview |

pairdeck remembers the directories you visit, ranked by frequency and recency like
[zoxide](https://github.com/ajeetdsouza/zoxide). **Alt-J** opens a fuzzy list
of them; on the command line, `z proj pair` jumps to the best match (the last
word must match the directory name) and `zi` opens the list.

## Terminal pane

**Alt-Z** opens a real shell (your `$SHELL`, on a pseudo terminal) in a split
below the panels, starting in the active panel's directory. Full-screen
programs such as vim, less and htop work inside it, with 256 colors and true
color. Click **Terminal** on the menu bar to toggle it with the mouse.

| Key | Action |
|-----|--------|
| Alt-Z | Show / focus the terminal; pressed in the terminal: back to the panels |
| Ctrl-X Z | Maximize / restore (hides the panels) |
| Ctrl-X K | Hide the pane (the shell keeps running) |
| Ctrl-X + / Ctrl-X - | Taller / shorter; or drag the pane's top border |
| Shift-PgUp / Shift-PgDn | Scroll back (the mouse wheel too) |
| Ctrl-X [ | cd the shell to the active panel's directory |
| Ctrl-X ] | Move the active panel to the shell's current directory |
| Ctrl-X Ctrl-V | Type the selected paths into the shell |
| Ctrl-X Ctrl-Z | Show the shell as a panel (in the other panel) / back to the file list |

The shell can also live in one of the two panels instead of the bottom split:
Ctrl-X Ctrl-Z (or Left/Right menu > Terminal) docks it into the other panel,
next to your file list. Tab, Alt-Z or a click move focus between the shell and
the file panel, and `[x]` in its border brings the file list back. It is the
same shell session as the bottom pane, so history and running programs move
with it.

While the terminal is focused every key except Alt-Z goes to the shell.
Typing `exit` closes the pane. Options > Configuration has "Terminal pane follows
the panel directory" (sends `cd` when the shell is idle), and `terminal_shell` in
`config.toml` chooses another program. Ctrl-O (full-screen subshell) still works
as before.

## Mouse

Click to select, double-click to open, right-click to mark, wheel to scroll.
Every function key bar is clickable (panels, viewer, editor), and every
full-screen view and side view has an [x] close button in its top-right corner.

On macOS, Option+letter is treated as Alt+letter in the panels, so Alt shortcuts
work without changing terminal settings (`macos_option_as_alt = false` to turn
this off).

## Tabs and file operations

| Keys | Action |
|------|--------|
| Ctrl-X n / Ctrl-X w | New tab / close tab (each panel side has its own tabs) |
| Ctrl-X . / Ctrl-X , | Next / previous tab |
| Alt-1..9 | Jump to tab 1..9 (Option-1..9 on macOS); tabs in the border are clickable |
| Ctrl-X m | Bulk rename: edit the selected names as a list, review, apply (swaps and cycles are handled) |
| Ctrl-X e | Regex rename with a live preview: `$1` groups, apply to the whole name, the stem or the extension |
| Ctrl-X u | Undo the last operation: rename, move, copy (deletes the copies after asking), mkdir, symlink, bulk rename, move to trash |
| Ctrl-X U | Undo history: undo several steps at once |
| Ctrl-X = | Find duplicate files: size groups, then a partial and a full BLAKE3 hash; keep the newest/oldest, panelize, trash the rest |
| Ctrl-X b | Disk usage analyzer (ncdu-like): largest first with bars, browse, trash, apparent size vs disk usage |

Tabs are saved with the rest of the state and restored on the next start.
Directory listings run in the background: slow network mounts, huge folders and
SFTP show a spinner instead of freezing the screen, and Esc cancels.

Restoring from the trash: on Linux this uses the freedesktop trash; on macOS
pairdeck looks the item up in `~/.Trash`, or asks Finder to put it back when macOS
privacy protection hides that folder from the terminal.

## Directory sync

**Ctrl-X Y** (Command menu > Synchronize directories) compares the left and
right panel directories recursively, through their file systems, so local,
archive, SFTP, S3 and WebDAV sides all work.

- Direction: left -> right, right -> left, or two-way where the newer file wins
  (equal times with different content become conflicts you resolve).
- Compare by size and modification time (2 s tolerance for FAT and remotes),
  size only, or content (BLAKE3 checksum).
- Mirror mode deletes files on the target that the source does not have (to the
  trash for local files, after a confirmation).
- Include / exclude globs (default exclude `.git;.DS_Store`); excluded files
  are never touched, even inside deleted directories.
- Dry run shows the plan only.

Nothing changes until you execute the plan. In the plan screen:

| Key | Action |
|-----|--------|
| Space | Turn an item on or off |
| Right / Left | Left wins (copy to the right) / right wins (copy to the left); deletes for one-sided items |
| s / r | Skip the item / reset it to the computed action |
| a | Toggle every item in the current view |
| Tab, 1-6 | Views: all, to the right, to the left, deletes, conflicts, skipped |
| Enter or F2 | Execute as a background job with progress (Esc closes without changes) |

New copies, created directories and trashed files can be undone with Ctrl-X u.

Press F1 inside the program for the full, live key binding list.

## Image actions

Select images (Insert marks several) and press **Ctrl-X g** for the Image
actions list, or use **Alt-R** / **Alt-L** to rotate right / left directly. The
same actions are in the File menu.

| Action | Notes |
|--------|-------|
| Rotate 90 / 180, flip | JPEG: only the EXIF orientation tag is rewritten, so there is no quality loss and the compressed data is untouched. Other formats are re-encoded. |
| Resize | Fit within W x H, exact size, percent, or fill (crop the center to the aspect ratio, then scale). "Only shrink" leaves smaller images alone. Lanczos3, Catmull-Rom, bilinear or nearest filters. |
| Convert | PNG, JPEG (quality), WebP (lossless only, the image crate has no lossy WebP encoder), BMP, TIFF, GIF (single frame), QOI, ICO (scaled to fit 256x256). SVG input is rendered at a chosen size. Transparency becomes white in JPEG. |
| Strip metadata | Re-encodes without EXIF, ICC and text chunks. |
| Auto-orient | Applies the EXIF orientation to the pixels and resets the tag. |

Output: overwrite the original, add a suffix (`{w}` and `{h}` become the new
size, default `_{w}x{h}`), or write into a directory (relative to each image).
Existing files are never clobbered: `name (1).png` is used instead. Every
re-encode bakes the EXIF orientation into the pixels and keeps the other EXIF
and ICC data (JPEG, PNG, WebP), so results look exactly like the preview.

Files are written to a temporary file next to the target and renamed into
place, so an error never leaves a half-written image. Overwritten originals
are kept as backups for the session: **Ctrl-X u** restores them and removes new
files. Animated GIFs are refused (only the first frame would survive). Options:
`image_lossless_jpeg_rotate = true` and `image_jpeg_quality = 90` in
`config.toml`.

## Remote file systems and archives

**Ctrl-X Ctrl-R** (or Left/Right menu > Remote link) connects to SFTP, S3 or
WebDAV; the same works from the command line:

| URL | Notes |
|-----|-------|
| `sftp://user@host:port/path` | ssh-agent, `~/.ssh/config` aliases, keys, or a password in the dialog; host keys checked against `~/.ssh/known_hosts` |
| `s3://bucket/prefix` | Credentials from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`, or `AWS_PROFILE` in `~/.aws/credentials`; region and `endpoint_url` from `AWS_REGION` / `AWS_ENDPOINT_URL` / `~/.aws/config`. The dialog also takes an endpoint (MinIO, R2, Ceph, rclone), explicit keys, or anonymous access. Large uploads use multipart. |
| `davs://user:pass@host/path` | WebDAV over https (`dav://` for http; `webdav(s)://` also accepted), HTTP Basic auth |

Directories on S3 are key prefixes (`mkdir` writes a `dir/` marker); S3 and
WebDAV have no permissions or symlinks.

Local **zip, tar, tar.gz, tar.bz2, tar.xz, tar.zst and 7z** archives are
writable: copy files in with F5, create directories, delete, chmod and create
symlinks inside them. Changes are collected and the archive is rewritten to a
temp file next to it, then atomically renamed over the original when the file
operation finishes (or when you leave the archive). Single-file `.gz` / `.bz2`
/ `.xz` / `.zst` and archives nested in other archives or on remote hosts stay
read-only. RAR is not supported (no permissively licensed pure-Rust reader).

### Encrypted zip files, ISO and DMG images

- **Password-protected zip** (ZipCrypto and AES-128/192/256): encrypted entries
  show a lock. Opening, viewing, editing or copying them (F3, F4, F5, Enter)
  asks for the password first; a wrong password asks again. The password is
  checked by fully decrypting an entry and is remembered in memory for the
  rest of the session only (never written to disk or logged). Encrypted zip
  files are read-only in pairdeck.
- **ISO images** (`.iso`, and `.img` files with an ISO 9660 signature) are
  browsed read-only on every platform by pairdeck's own parser: Joliet and Rock
  Ridge long names, permissions, symlinks, relocated directories and
  multi-extent files. File data is streamed from the image, never loaded whole.
- **Disk images** (`.dmg`) on macOS: Enter attaches the image read-only with
  `hdiutil` (no Finder window, private mount point; a password dialog for
  encrypted images, passed to hdiutil on stdin) and the panel browses it.
  Images with several volumes show one directory per volume. The image is
  detached when no panel shows it any more and when pairdeck exits. On other
  systems `.dmg` files are listed and extracted through 7-Zip when it is
  installed. The quick view shows `hdiutil imageinfo` details without mounting.

Integration tests run every backend against real servers:

```sh
scripts/test-remote.sh            # sftp + s3 + webdav
scripts/test-remote.sh s3         # one backend
```

They use a throwaway non-root `sshd` and `rclone serve s3|webdav` when
available, otherwise Docker (openssh-server, MinIO, rclone images;
`PAIRDECK_FORCE_DOCKER=1` forces it). Nothing touches your real `~/.ssh` or
`~/.aws`. To test against your own servers, see the `PAIRDECK_TEST_*` variables at
the top of `tests/*_integration.rs`.

## Development

```sh
cargo fmt --all            # format (rustfmt.toml)
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked        # unit tests + UI flow tests (tests/)
cargo audit                # RustSec advisories (.cargo/audit.toml)
cargo deny check           # advisories, licenses, bans, sources (deny.toml)
./fuzz/run.sh              # replay regressions, then fuzz every target for 60s
./fuzz/run.sh archive 600  # fuzz one target longer
```

Git hooks are managed by [cargo-husky](https://github.com/rhysd/cargo-husky) and
installed into `.git/hooks` on the first `cargo test`. CI runs the same commands:

| Where | Checks |
|-------|--------|
| pre-commit hook | `cargo fmt --all -- --check`, `cargo clippy --all-targets --locked -- -D warnings` |
| pre-push hook | `cargo test --locked`, `cargo audit`, `cargo deny check` |
| CI (`.github/workflows/ci.yml`, Linux + macOS) | all of the above; pull requests replay the fuzz regressions, the nightly run fuzzes every target for 60 s |
| Release (`.github/workflows/release.yml`) | the full CI, then static musl binaries for x86_64/aarch64 Linux, macOS arm64/x86_64 and a universal binary, `.deb` packages, build attestations, a GitHub release with `sha256sums.txt`, the Homebrew tap and crates.io |

### Releasing

1. Bump `version` in `Cargo.toml` (and `Cargo.lock` with `cargo check`), merge it to `main`.
2. Actions > Release > Run workflow, enter the same version (e.g. `0.2.0`), or run
   `gh workflow run release.yml -f version=0.2.0`.

The workflow refuses a version that does not match `Cargo.toml` or whose tag exists,
runs the whole CI, builds and test-installs every package, and only then creates the tag
`v0.2.0` and the GitHub release (pushing that tag yourself works too). A version with a
suffix such as `0.2.0-rc.1` becomes a pre-release and skips the Homebrew tap.

The Homebrew formula is rendered from `packaging/homebrew/pairdeck.rb.in` by
`scripts/homebrew-formula.sh` and pushed to `portvex/homebrew-tap` with a deploy key
(secret `HOMEBREW_TAP_DEPLOY_KEY`); crates.io needs the secret `CARGO_REGISTRY_TOKEN`.
Without a secret that step is skipped with a warning.

### UI tests

`tests/ui_flows.rs` and `tests/ui_config.rs` drive the real application without
a terminal: `tests/common/mod.rs` builds an `App`, renders it into a ratatui
`TestBackend`, sends real key and mouse events and pumps the event channel
(background jobs, previews, worker callbacks) until a condition holds. Flows
covered include navigation, copy with an overwrite conflict, move, rename,
nested mkdir, delete, marking, quick search, archives, quick view, the menu,
mouse clicks, viewer, editor, hotlist, `cd`, panelize, live reload and the key
binding editor. Tests point the config and state directories at a temp dir
(`pairdeck::config::override_dirs`), so your real settings are never touched.

On Linux the suite can be run in a container:

```sh
docker run --rm -v "$PWD":/src -v pairdeck-cargo:/usr/local/cargo/registry \
  -v pairdeck-target:/target -e CARGO_TARGET_DIR=/target -w /src \
  rust:1-bookworm cargo test --locked
```

### Screenshots

`scripts/screenshots.sh` regenerates the images in `docs/screenshots/`. It
drives the real app over this repository (`tests/screenshots.rs`, ignored by a
normal `cargo test`), renders each screen to HTML and photographs it with
headless Chrome, using the JetBrainsMono Nerd Font. Your user and host names are
replaced by `demo@laptop`, so the images are safe to publish.

Fuzzing uses libFuzzer through [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz)
and needs a nightly toolchain (`rustup toolchain install nightly`,
`cargo install cargo-fuzz`). Targets in `fuzz/fuzz_targets/`:

| Target | Input under test |
|--------|------------------|
| `archive` | Untrusted zip / tar / tar.gz / tar.bz2 / tar.xz / tar.zst / 7z / gz / bz2 / xz / zst files: open, list, read, and write + commit |
| `webdav_xml` | PROPFIND multistatus responses from WebDAV servers, WebDAV URLs |
| `s3_xml` | ListObjectsV2 and CreateMultipartUpload responses from S3 servers, `s3://` URLs |
| `sftp_target` | `sftp://user@host:port/path` strings typed on the command line |
| `user_menu` | MC-format user menu files (including a per-directory `.pairdeck.menu`) |
| `config` | `config.toml` and `state.toml`, key bindings, themes |
| `keymap` | Key binding and style strings |
| `line_edit` | Arbitrary key sequences in the line editor |
| `text_utils` | File-name truncation, quoting, masks, natural sort, globs |
| `svg` | Untrusted SVG / SVGZ files rendered in the preview and viewer |
| `iso` | Untrusted ISO 9660 images (Joliet, Rock Ridge, CE continuation areas, multi-extent files) |
| `table` | CSV / TSV tables and plain-text highlighting |
| `json_tree` | JSON parsing, pretty printing, tree navigation; TOML validation |
| `media_meta` | Photo EXIF, font files, executables, animated GIF / APNG / WebP |
| `docs` | Office documents and e-books (zip + XML text extraction) |

Inputs that once crashed are kept in `fuzz/regressions/<target>/` and replayed by
`./fuzz/run.sh replay`; `fuzz/seeds/` holds the starting corpus.
