//! Produce a `LOCAL_ADMIN_PASSWORD_HASH` for a self-hosted install.
//!
//! Replaces `backend/scripts/hash_local_admin_password.py`, which
//! `AGENTS.md` tells a self-hoster to run and which goes when the Python
//! does. Without it there is no way to generate the one credential a
//! self-hosted deployment cannot start without.
//!
//! ```
//! sentinel-hash-password            # prompts, echo off
//! echo -n 'secret' | sentinel-hash-password --stdin
//! ```
//!
//! **The parameters are python-argon2's, not the Rust crate's default.**
//! `Argon2::default()` in the `argon2` crate is `m=19456, t=2, p=1`;
//! python-argon2's `PasswordHasher()` — which wrote every existing
//! `LOCAL_ADMIN_PASSWORD_HASH` — uses `m=65536, t=3, p=4`. Verification
//! is unaffected either way, because a PHC string carries its own
//! parameters and `verify_password` reads them from the stored hash. But
//! taking the crate default here would quietly issue weaker hashes than
//! the installs that came before, on the tool whose entire job is to
//! produce a credential. So the stronger set is set explicitly.

use std::io::{IsTerminal, Read, Write};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHasher, SaltString};

/// python-argon2's `PasswordHasher()` defaults.
const MEMORY_KIB: u32 = 65_536;
const ITERATIONS: u32 = 3;
const PARALLELISM: u32 = 4;

/// Printed by `--help`.
///
/// It exists because the argument loop ignores anything it does not
/// recognise, so `--help` would otherwise sit at a password prompt with no
/// output — the one response worse than an error.
const USAGE: &str = "\
usage: sentinel-hash-password [--stdin]

Print a LOCAL_ADMIN_PASSWORD_HASH for a self-hosted install
(AUTH_PROVIDER=local). Argon2id with python-argon2's parameters, so a hash
written by the tool this replaces still verifies.

  --stdin     Read the password from stdin instead of prompting, for a
              provisioning script:  echo -n 'secret' | sentinel-hash-password --stdin
  --help, -h  This text.

With no flag it prompts twice with echo off. Nothing is written anywhere:
copy the printed line into the environment yourself.
";

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    if let Some(unknown) = args.iter().find(|a| *a != "--stdin") {
        eprintln!("unknown argument: {unknown}\n");
        eprint!("{USAGE}");
        return std::process::ExitCode::from(2);
    }
    let from_stdin = args.iter().any(|a| a == "--stdin");

    let password = match read_password(from_stdin) {
        Ok(password) => password,
        Err(message) => {
            eprintln!("{message}");
            return std::process::ExitCode::from(1);
        }
    };
    if password.is_empty() {
        eprintln!("Password must not be empty.");
        return std::process::ExitCode::from(1);
    }

    let params = match argon2::Params::new(MEMORY_KIB, ITERATIONS, PARALLELISM, None) {
        Ok(params) => params,
        Err(err) => {
            eprintln!("argon2 parameters rejected: {err}");
            return std::process::ExitCode::from(1);
        }
    };
    let hasher = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let salt = SaltString::generate(&mut OsRng);
    let hash = match hasher.hash_password(password.as_bytes(), &salt) {
        Ok(hash) => hash.to_string(),
        Err(err) => {
            eprintln!("hashing failed: {err}");
            return std::process::ExitCode::from(1);
        }
    };

    println!("\nLOCAL_ADMIN_PASSWORD_HASH={hash}");
    std::process::ExitCode::SUCCESS
}

/// Read the password, twice and with echo off when there is a terminal.
///
/// `--stdin` reads one line instead, which is what a provisioning script
/// wants and what the Python tool had no way to offer.
fn read_password(from_stdin: bool) -> Result<String, String> {
    if from_stdin || !std::io::stdin().is_terminal() {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|err| format!("could not read stdin: {err}"))?;
        // A trailing newline from `echo` is not part of the password, and
        // silently hashing one produces a credential nobody can type.
        return Ok(buffer.trim_end_matches(['\n', '\r']).to_string());
    }

    let first = prompt("New local admin password: ")?;
    let second = prompt("Confirm password: ")?;
    if first != second {
        return Err("Passwords did not match.".to_string());
    }
    Ok(first)
}

fn prompt(label: &str) -> Result<String, String> {
    print!("{label}");
    std::io::stdout().flush().ok();
    let _guard = EchoOff::new();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|err| format!("could not read the password: {err}"))?;
    println!();
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// Terminal echo, off for the lifetime of the value.
///
/// Done with `termios` through the `libc` this crate already depends on
/// (for `statvfs` in the disk probe) rather than adding a dependency for
/// it. The guard restores the old settings on drop INCLUDING on the
/// error paths above — a tool that exits with echo still disabled leaves
/// the operator's terminal silently broken.
struct EchoOff {
    restore: Option<libc::termios>,
}

impl EchoOff {
    fn new() -> Self {
        let fd = libc::STDIN_FILENO;
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `term` is a fully owned, correctly sized buffer and
        // STDIN_FILENO is always a valid descriptor number; tcgetattr
        // reports failure rather than writing to it when stdin is not a
        // terminal.
        if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
            return Self { restore: None };
        }
        let original = term;
        term.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
        Self { restore: Some(original) }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        if let Some(original) = self.restore {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
        }
    }
}
