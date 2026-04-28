/// `amux current` — print `$AMUX_SESSION` to stdout.
///
/// Pure-stdlib helper: no daemon roundtrip, no socket. Used by slash
/// commands and shell snippets to discover whether they're inside an
/// amux session and, if so, which one. Exits 0 on success, 1 (with a
/// stderr message) when unset.
pub fn do_current() -> anyhow::Result<()> {
    match std::env::var("AMUX_SESSION") {
        Ok(name) if !name.is_empty() => {
            println!("{}", name);
            Ok(())
        }
        _ => {
            eprintln!("amux: not running inside an amux session");
            std::process::exit(1);
        }
    }
}
