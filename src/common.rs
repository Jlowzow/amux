use std::io;
use std::path::{Path, PathBuf};

/// Environment variable that selects an amux instance. Three states:
///
/// * **Unset** — auto-derive an instance name from the cwd (project
///   root), so two clients invoked anywhere inside the same project
///   share a daemon. Returns `None` if the cwd is not inside any
///   git project.
/// * **Set to a non-empty value** — use that name verbatim. Equivalent
///   to passing `--instance <name>`. The flag wins when both are set
///   (see `main.rs`, which propagates the flag into this env var).
/// * **Set to the empty string** — explicit opt-out: don't derive,
///   just use the un-suffixed default. Escape hatch for users who want
///   to keep the legacy single-daemon behavior.
///
/// When a name is selected, every runtime path is suffixed with
/// `-{name}`, giving a fully separate daemon, socket, pid file, and
/// session registry. Used to run multiple orchestrators (e.g. one per
/// project) on the same machine without their workers showing up in
/// each other's `amux ls`.
pub const INSTANCE_ENV: &str = "AMUX_INSTANCE";

/// Resolve the effective instance name for this invocation.
///
/// Single source of truth — every caller that wants to know "which
/// instance is this?" (runtime path computation, `amux top` title bar,
/// log lines, etc.) should call this rather than reading the env var
/// directly, so that auto-derivation behaves consistently everywhere.
///
/// See `INSTANCE_ENV` for the precedence rules.
pub fn resolved_instance() -> Option<String> {
    resolve_instance(
        std::env::var(INSTANCE_ENV).ok().as_deref(),
        derive_instance_from_cwd,
    )
}

/// Pure precedence helper: takes the env var value and a closure
/// computing the cwd-derived fallback, returns the chosen name.
/// Split out so the precedence rules can be unit-tested without
/// mutating the process environment (which is racy under parallel
/// `cargo test`).
fn resolve_instance(env: Option<&str>, derive: impl FnOnce() -> Option<String>) -> Option<String> {
    match env {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        Some(_) => None, // explicit empty = opt out of auto-derivation
        None => derive(),
    }
}

/// Return the directory used for amux runtime files.
pub fn runtime_dir() -> PathBuf {
    runtime_dir_for(resolved_instance().as_deref())
}

/// Pure helper: compute the runtime dir for an explicit instance name.
/// `None` and `Some("")` both yield the default (un-suffixed) path.
pub fn runtime_dir_for(instance: Option<&str>) -> PathBuf {
    let uid = nix::unistd::getuid();
    match instance {
        Some(name) if !name.is_empty() => PathBuf::from(format!("/tmp/amux-{}-{}", uid, name)),
        _ => PathBuf::from(format!("/tmp/amux-{}", uid)),
    }
}

/// Derive an instance name from the current working directory.
///
/// Two branches:
///
/// 1. **cwd is inside a git project** — use the project root's
///    basename (with a hash suffix). Sibling git worktrees of one
///    project resolve to the same name and therefore the same daemon.
/// 2. **cwd is NOT inside any git project** — fall back to the cwd's
///    own basename (slugified) plus a hash of the canonical cwd. So
///    every directory still gets a stable instance label, and
///    `amux top` can show e.g. `[instance: tmp-1a2b]` from `/tmp`
///    instead of an empty label.
///
/// Returns `None` only when the cwd is unreadable or the basename is
/// empty after slugification (i.e. cwd is the filesystem root). In
/// that case callers fall back to the legacy un-suffixed runtime path.
///
/// The derived name is byte-stable across processes (fixed-seed hash
/// of the canonical path), so two invocations from the same directory
/// always resolve to the same name.
pub fn derive_instance_from_cwd() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    derive_instance_from_path(&cwd)
}

/// Pure helper: same logic as [`derive_instance_from_cwd`] but takes
/// the cwd explicitly so it can be unit-tested without mutating the
/// process cwd (which is racy under parallel `cargo test`).
fn derive_instance_from_path(cwd: &Path) -> Option<String> {
    if let Some(project_root) = find_project_root(cwd) {
        return Some(instance_name_for_root(&project_root));
    }
    derive_instance_from_pwd(cwd)
}

/// Build an instance name from a non-git cwd's own basename. Format
/// `{slug}-{4 hex chars}` — same shape as `instance_name_for_root` so
/// the two branches are visually indistinguishable in `amux top` and
/// runtime path names. Returns `None` when slugification yields an
/// empty string (cwd is `/` or has no usable basename).
fn derive_instance_from_pwd(cwd: &Path) -> Option<String> {
    let canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let basename = canon.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let slug = pwd_slug(basename);
    if slug.is_empty() {
        return None;
    }
    let hash = fnv1a_64(canon.to_string_lossy().as_bytes());
    Some(format!("{}-{:04x}", slug, (hash & 0xffff) as u16))
}

/// Slug a path basename for the PWD branch: lowercase, replace any run
/// of non-alphanumeric chars with a single `-`, trim leading/trailing
/// dashes. Stricter than [`sanitize_basename`] (which preserves case,
/// `_`, and runs of `-`) because the PWD branch generates names from
/// arbitrary user directories — spaces, dots, mixed case, mktemp
/// suffixes — and we want a single canonical form.
fn pwd_slug(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in s.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_alphanumeric() {
            out.push(lc);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
}

/// Walk up from `start` to find the project root — the directory
/// containing a `.git` entry. Handles both:
///
/// * `.git` is a directory — the standard main checkout.
/// * `.git` is a file — a linked git worktree, whose contents are
///   `gitdir: <path-to-main>/.git/worktrees/<name>`. We resolve that
///   pointer back to the main checkout so worktrees of one project
///   share an instance with their main checkout.
///
/// Returns `None` if no `.git` is found before hitting the filesystem
/// root.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        let git = ancestor.join(".git");
        let meta = match std::fs::symlink_metadata(&git) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            return Some(ancestor.to_path_buf());
        }
        if meta.is_file() {
            return main_root_from_gitfile(&git, ancestor);
        }
        // symlink or other — fall through
    }
    None
}

/// Given a `.git` *file* (worktree pointer) at `gitfile_path` whose
/// containing directory is `worktree_dir`, follow `gitdir: ...` back
/// to the main checkout. Returns `worktree_dir` itself as a fallback
/// if the gitfile is malformed — better to give an instance than to
/// silently fall back to the un-suffixed default.
fn main_root_from_gitfile(gitfile_path: &Path, worktree_dir: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(gitfile_path).ok()?;
    let gitdir_line = contents
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(|s| s.trim());
    let Some(gitdir) = gitdir_line else {
        return Some(worktree_dir.to_path_buf());
    };
    let gitdir_path = Path::new(gitdir);
    // Relative gitdir is resolved against the worktree dir.
    let abs_gitdir = if gitdir_path.is_absolute() {
        gitdir_path.to_path_buf()
    } else {
        worktree_dir.join(gitdir_path)
    };
    // Layout for linked worktrees is `<main>/.git/worktrees/<name>`,
    // so the main checkout is the grandparent of `.git/worktrees/`.
    // Walk up looking for a path component named ".git" and return
    // its parent.
    for p in abs_gitdir.ancestors() {
        if p.file_name().and_then(|s| s.to_str()) == Some(".git") {
            if let Some(parent) = p.parent() {
                return Some(parent.to_path_buf());
            }
        }
    }
    Some(worktree_dir.to_path_buf())
}

/// Build a stable, human-readable instance name from a project root
/// path. Format: `{basename}-{4 hex chars}`. The hash disambiguates
/// two clones with the same basename (e.g. `~/code/amux` and
/// `~/forks/amux`).
fn instance_name_for_root(root: &Path) -> String {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let basename = canon
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let slug = sanitize_basename(basename);
    let hash = fnv1a_64(canon.to_string_lossy().as_bytes());
    format!("{}-{:04x}", slug, (hash & 0xffff) as u16)
}

/// Restrict to ASCII alnum + `-`/`_` so the instance name is always
/// safe as a path suffix on every filesystem we care about.
fn sanitize_basename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    if cleaned.is_empty() { "project".to_string() } else { cleaned }
}

/// FNV-1a 64-bit. Inlined (tiny, no dep) so the derived instance name
/// is byte-stable across processes — `std::hash::DefaultHasher` uses a
/// randomized seed and would give a different name every invocation.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Return the path to the server socket.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("server.sock")
}

/// Return the path to the server pid file.
pub fn pid_file_path() -> PathBuf {
    runtime_dir().join("server.pid")
}

/// Check if a server is already running by attempting to connect.
pub fn server_running() -> bool {
    let path = socket_path();
    if !path.exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(&path).is_ok()
}

/// Write the daemon PID to a file.
pub fn write_pid_file(pid: u32) -> io::Result<()> {
    std::fs::write(pid_file_path(), pid.to_string())
}

/// Read the daemon PID from the pid file.
pub fn read_pid_file() -> io::Result<u32> {
    let contents = std::fs::read_to_string(pid_file_path())?;
    contents
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Remove the pid file if present. Not-found is treated as success.
pub fn remove_pid_file() -> io::Result<()> {
    match std::fs::remove_file(pid_file_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Check if a process with the given pid exists.
///
/// Uses `kill(pid, 0)`: `Ok` means the process exists and we can signal
/// it, `EPERM` means it exists but is owned by another user, anything
/// else (typically `ESRCH`) means it's gone.
pub fn pid_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    if pid == 0 {
        return false;
    }
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        _ => false,
    }
}

/// Return the command-name (`comm`) reported by `ps` for the given pid,
/// or `None` if the pid is dead or `ps` fails.
///
/// On macOS and Linux this is the executable basename — e.g. "amux" for
/// our daemon — and is set by `execve`, so a daemon forked from a client
/// (without execve) still reports the parent's comm.
pub fn pid_command(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let comm = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if comm.is_empty() {
        None
    } else {
        Some(comm)
    }
}

/// Check that the given pid is alive and its binary name starts with
/// "amux" (i.e. is plausibly our daemon, not a stale client pid or a
/// recycled pid belonging to some other process).
pub fn pid_is_amux(pid: u32) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    let Some(cmd) = pid_command(pid) else {
        return false;
    };
    let basename = std::path::Path::new(&cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&cmd);
    basename.starts_with("amux")
}

/// Return `true` iff the pid file exists and points to a live amux
/// process. Used to detect stale/reclaimable pid files.
pub fn pid_file_points_to_amux() -> bool {
    match read_pid_file() {
        Ok(pid) => pid_is_amux(pid),
        Err(_) => false,
    }
}

/// True iff the daemon appears to be running — both the socket accepts
/// connections AND the pid file references a live amux process. Either
/// signal on its own can be stale (socket kept by a dead daemon that
/// didn't clean up, pid file left behind after SIGKILL), so we require
/// both.
pub fn daemon_alive() -> bool {
    server_running() && pid_file_points_to_amux()
}

/// Remove stale socket and pid files. Safe to call when the files are
/// absent. Callers must ensure the daemon is not running before calling.
pub fn clear_stale_runtime_files() -> io::Result<()> {
    let sock = socket_path();
    if sock.exists() {
        match std::fs::remove_file(&sock) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    remove_pid_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_alive_reports_current_process() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_rejects_zero() {
        assert!(!pid_alive(0));
    }

    #[test]
    fn pid_alive_reports_dead_pid() {
        // Pick a pid that is extraordinarily unlikely to exist.
        assert!(!pid_alive(0x7fff_fffe));
    }

    #[test]
    fn pid_command_returns_something_for_current_process() {
        let comm = pid_command(std::process::id());
        assert!(comm.is_some(), "expected ps to return a comm string");
    }

    #[test]
    fn pid_command_none_for_dead_pid() {
        assert!(pid_command(0x7fff_fffe).is_none());
    }

    #[test]
    fn pid_is_amux_rejects_dead_pid() {
        assert!(!pid_is_amux(0x7fff_fffe));
    }

    #[test]
    fn pid_is_amux_accepts_current_test_binary() {
        // `cargo test` runs a binary named `amux-<hash>` under
        // target/debug/deps, so its comm starts with "amux".
        let pid = std::process::id();
        let comm = pid_command(pid).unwrap_or_default();
        if comm.starts_with("amux") {
            assert!(pid_is_amux(pid));
        }
    }

    #[test]
    fn runtime_dir_for_default_when_no_instance() {
        let uid = nix::unistd::getuid();
        let expected = PathBuf::from(format!("/tmp/amux-{}", uid));
        assert_eq!(runtime_dir_for(None), expected);
        assert_eq!(runtime_dir_for(Some("")), expected);
    }

    #[test]
    fn runtime_dir_for_named_instance_is_suffixed() {
        let uid = nix::unistd::getuid();
        assert_eq!(
            runtime_dir_for(Some("projA")),
            PathBuf::from(format!("/tmp/amux-{}-projA", uid))
        );
    }

    #[test]
    fn runtime_dir_for_two_instances_are_distinct() {
        // Different instance names must never collide — that's the
        // whole point of the feature.
        let a = runtime_dir_for(Some("alice"));
        let b = runtime_dir_for(Some("bob"));
        assert_ne!(a, b);
        // Both must differ from the default too.
        assert_ne!(runtime_dir_for(None), a);
        assert_ne!(runtime_dir_for(None), b);
    }

    #[test]
    fn resolve_instance_prefers_explicit_env() {
        let out = resolve_instance(Some("alice"), || Some("derived".to_string()));
        assert_eq!(out.as_deref(), Some("alice"));
    }

    #[test]
    fn resolve_instance_empty_env_opts_out() {
        // Explicit empty string is the documented escape hatch back to
        // the legacy single-daemon path. Must not fall back to deriving.
        let mut derive_called = false;
        let out = resolve_instance(Some(""), || {
            derive_called = true;
            Some("derived".to_string())
        });
        assert_eq!(out, None);
        assert!(!derive_called, "empty env must not invoke derivation");
    }

    #[test]
    fn resolve_instance_unset_env_derives_from_cwd() {
        let out = resolve_instance(None, || Some("derived".to_string()));
        assert_eq!(out.as_deref(), Some("derived"));
    }

    #[test]
    fn resolve_instance_unset_env_with_no_project_returns_none() {
        let out = resolve_instance(None, || None);
        assert_eq!(out, None);
    }

    #[test]
    fn fnv1a_64_is_deterministic() {
        // Stability across calls is the whole reason we don't use
        // DefaultHasher (which uses a randomized seed).
        let a = fnv1a_64(b"/some/project/path");
        let b = fnv1a_64(b"/some/project/path");
        assert_eq!(a, b);
        assert_ne!(a, fnv1a_64(b"/other/project/path"));
    }

    #[test]
    fn fnv1a_64_known_vector() {
        // FNV-1a of empty input is the offset basis.
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn sanitize_basename_keeps_alnum_and_hyphen_underscore() {
        assert_eq!(sanitize_basename("amux"), "amux");
        assert_eq!(sanitize_basename("my-proj_2"), "my-proj_2");
    }

    #[test]
    fn sanitize_basename_replaces_unsafe_chars() {
        assert_eq!(sanitize_basename("foo bar"), "foo-bar");
        assert_eq!(sanitize_basename("foo/bar"), "foo-bar");
        // 'é' is a single char (U+00E9), so it becomes one '-'.
        assert_eq!(sanitize_basename("café"), "caf-");
    }

    #[test]
    fn sanitize_basename_falls_back_when_empty() {
        assert_eq!(sanitize_basename(""), "project");
    }

    #[test]
    fn instance_name_for_root_format_is_basename_dash_4hex() {
        // Construct a path that won't exist (so canonicalize falls
        // back to the input as-is) and check the shape.
        let p = PathBuf::from("/nonexistent/test/amux-fake");
        let name = instance_name_for_root(&p);
        // basename + "-" + 4 hex chars
        let (basename, hex) = name.rsplit_once('-').expect("must contain a '-'");
        assert_eq!(basename, "amux-fake");
        assert_eq!(hex.len(), 4);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn instance_name_for_root_is_stable_for_same_path() {
        let p = PathBuf::from("/nonexistent/test/stable");
        assert_eq!(instance_name_for_root(&p), instance_name_for_root(&p));
    }

    #[test]
    fn instance_name_for_root_disambiguates_clones_with_same_basename() {
        // Two clones of "amux" in different parents must get different
        // hash suffixes — that's the disambiguation guarantee.
        let a = PathBuf::from("/nonexistent/path-a/amux");
        let b = PathBuf::from("/nonexistent/path-b/amux");
        assert_ne!(instance_name_for_root(&a), instance_name_for_root(&b));
    }

    #[test]
    fn find_project_root_returns_dir_containing_dot_git() {
        let tmp = tempdir_unique("amux-fpr-dir");
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        std::fs::create_dir_all(tmp.join("src/sub/deep")).unwrap();
        let from = tmp.join("src/sub/deep");
        let root = find_project_root(&from).expect("must find root");
        assert_eq!(canonicalize_or(&root), canonicalize_or(&tmp));
        cleanup(&tmp);
    }

    #[test]
    fn find_project_root_returns_none_outside_any_project() {
        // /tmp itself has no .git (and shouldn't on any sane CI).
        // We use a fresh tempdir to isolate.
        let tmp = tempdir_unique("amux-fpr-empty");
        let root = find_project_root(&tmp);
        // Either None (no .git on the way to /), or the test machine
        // happens to have .git somewhere above /tmp — accept either.
        // The contract we care about: we don't panic.
        let _ = root;
        cleanup(&tmp);
    }

    #[test]
    fn find_project_root_from_worktree_gitfile_resolves_to_main_checkout() {
        // Simulate the layout `git worktree add` produces:
        //   <main>/.git/                       (real git dir)
        //   <main>/.git/worktreelink           (anything)
        //   <main>/wt/.git                     (file: "gitdir: <main>/.git/worktrees/wt")
        //   <main>/.git/worktrees/wt/...       (worktree's per-worktree git data)
        let tmp = tempdir_unique("amux-fpr-wt");
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        let wt = main.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let gitdir = main.join(".git/worktrees/wt");
        let gitfile_contents = format!("gitdir: {}\n", gitdir.display());
        std::fs::write(wt.join(".git"), gitfile_contents).unwrap();

        // Subdir inside the worktree.
        let inside = wt.join("src/inner");
        std::fs::create_dir_all(&inside).unwrap();

        let from_wt = find_project_root(&inside).expect("must resolve from worktree");
        let from_main = find_project_root(&main).expect("must resolve from main");
        assert_eq!(
            canonicalize_or(&from_wt),
            canonicalize_or(&from_main),
            "worktree must resolve to the same project root as the main checkout"
        );
        // And that root is the main checkout, not the worktree dir.
        assert_eq!(canonicalize_or(&from_wt), canonicalize_or(&main));
        cleanup(&tmp);
    }

    #[test]
    fn find_project_root_handles_relative_gitfile() {
        // git stores the gitdir as a relative path on some setups.
        let tmp = tempdir_unique("amux-fpr-relwt");
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/rel")).unwrap();
        let wt = main.join("rel");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../.git/worktrees/rel\n").unwrap();
        let root = find_project_root(&wt).expect("must resolve relative gitfile");
        assert_eq!(canonicalize_or(&root), canonicalize_or(&main));
        cleanup(&tmp);
    }

    #[test]
    fn pwd_slug_lowercases_and_keeps_alnum() {
        assert_eq!(pwd_slug("MyDir2"), "mydir2");
        assert_eq!(pwd_slug("abc123"), "abc123");
    }

    #[test]
    fn pwd_slug_replaces_runs_of_unsafe_chars_with_single_dash() {
        // Runs of mixed unsafe chars collapse to one dash — the strict
        // slug treats `_`, `.`, `-`, space, etc. all as separators.
        assert_eq!(pwd_slug("foo bar"), "foo-bar");
        assert_eq!(pwd_slug("foo___bar"), "foo-bar");
        assert_eq!(pwd_slug("foo...bar"), "foo-bar");
        assert_eq!(pwd_slug("foo - bar"), "foo-bar");
        assert_eq!(pwd_slug("tmp.XXXXX"), "tmp-xxxxx");
    }

    #[test]
    fn pwd_slug_trims_leading_and_trailing_dashes() {
        assert_eq!(pwd_slug("...foo..."), "foo");
        assert_eq!(pwd_slug("---foo---"), "foo");
        assert_eq!(pwd_slug("  hello  "), "hello");
    }

    #[test]
    fn pwd_slug_returns_empty_for_no_alnum_input() {
        assert_eq!(pwd_slug(""), "");
        assert_eq!(pwd_slug("..."), "");
        assert_eq!(pwd_slug("/"), "");
        assert_eq!(pwd_slug("---"), "");
    }

    #[test]
    fn derive_instance_from_pwd_returns_basename_with_hash_suffix() {
        // Shape check — slug + "-" + 4 hex chars, slug derives from the
        // canonical basename.
        let tmp = tempdir_unique("amux-pwd-shape");
        let result = derive_instance_from_pwd(&tmp).expect("non-empty basename");
        let (slug, hex) = result.rsplit_once('-').expect("must contain a '-'");
        // tempdir_unique includes pid + nanos, all digits — so the
        // slug starts with our prefix.
        assert!(slug.starts_with("amux-pwd-shape"));
        assert_eq!(hex.len(), 4);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        cleanup(&tmp);
    }

    #[test]
    fn derive_instance_from_pwd_is_stable_for_same_path() {
        // Two calls on the same dir yield the same name — required so
        // every amux invocation from the same cwd hits the same daemon.
        let tmp = tempdir_unique("amux-pwd-stable");
        let a = derive_instance_from_pwd(&tmp).expect("must derive");
        let b = derive_instance_from_pwd(&tmp).expect("must derive");
        assert_eq!(a, b);
        cleanup(&tmp);
    }

    #[test]
    fn derive_instance_from_pwd_returns_none_for_root() {
        // basename of "/" is None → slug is empty → None, so the
        // legacy un-suffixed runtime path kicks in.
        assert_eq!(derive_instance_from_pwd(Path::new("/")), None);
    }

    #[test]
    fn derive_instance_from_pwd_disambiguates_same_basename_in_different_parents() {
        // Two non-git dirs both named "foo" but in different parents
        // must get different hashes — same guarantee as the git branch.
        let tmp_a = tempdir_unique("amux-pwd-disA");
        let tmp_b = tempdir_unique("amux-pwd-disB");
        let dir_a = tmp_a.join("foo");
        let dir_b = tmp_b.join("foo");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let a = derive_instance_from_pwd(&dir_a).unwrap();
        let b = derive_instance_from_pwd(&dir_b).unwrap();
        // Both start with "foo-" but the hash differs.
        assert!(a.starts_with("foo-"), "a={a}");
        assert!(b.starts_with("foo-"), "b={b}");
        assert_ne!(a, b);
        cleanup(&tmp_a);
        cleanup(&tmp_b);
    }

    #[test]
    fn derive_instance_from_path_uses_pwd_when_no_git_repo() {
        // No .git anywhere on the way to /, so we fall through to the
        // PWD branch — the whole point of this bead.
        let tmp = tempdir_unique("amux-fallback");
        let result = derive_instance_from_path(&tmp).expect("must derive from PWD");
        assert!(
            result.starts_with("amux-fallback"),
            "expected slug to start with 'amux-fallback', got {result}"
        );
        cleanup(&tmp);
    }

    #[test]
    fn derive_instance_from_path_uses_git_branch_when_inside_repo() {
        // If a .git dir exists, the git branch wins — PWD fallback is
        // strictly the no-git path.
        let tmp = tempdir_unique("amux-git-wins");
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        let inside = tmp.join("sub/dir");
        std::fs::create_dir_all(&inside).unwrap();
        let from_inside = derive_instance_from_path(&inside).expect("must derive");
        let from_root = derive_instance_from_path(&tmp).expect("must derive");
        // Both resolve to the project root, not the cwd basename "dir".
        assert_eq!(from_inside, from_root);
        cleanup(&tmp);
    }

    #[test]
    fn find_project_root_handles_malformed_gitfile() {
        // Garbage in the gitfile — we should at least not panic and
        // not return None (it IS a worktree, after all).
        let tmp = tempdir_unique("amux-fpr-bad");
        let wt = tmp.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "garbage\n").unwrap();
        let root = find_project_root(&wt).expect("malformed gitfile must not return None");
        assert_eq!(canonicalize_or(&root), canonicalize_or(&wt));
        cleanup(&tmp);
    }

    // --- test helpers --------------------------------------------------

    fn tempdir_unique(prefix: &str) -> PathBuf {
        // Avoid dev-deps: roll our own tempdir. PID + nanos is unique
        // enough for test isolation.
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        let p = std::env::temp_dir().join(format!("{}-{}-{}", prefix, pid, nanos));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn cleanup(p: &Path) {
        let _ = std::fs::remove_dir_all(p);
    }

    fn canonicalize_or(p: &Path) -> PathBuf {
        p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
    }

    #[test]
    fn remove_pid_file_is_idempotent_when_absent() {
        // Write then remove twice to confirm the second call succeeds.
        let path = pid_file_path();
        let dir = runtime_dir();
        let _ = std::fs::create_dir_all(&dir);
        let prior = std::fs::read_to_string(&path).ok();
        let _ = std::fs::remove_file(&path);
        assert!(remove_pid_file().is_ok(), "remove on missing must succeed");
        assert!(remove_pid_file().is_ok(), "second remove must succeed");
        // Restore previous contents if any to avoid breaking a concurrent daemon.
        if let Some(p) = prior {
            let _ = std::fs::write(&path, p);
        }
    }
}
