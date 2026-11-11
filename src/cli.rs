//! Command-line backup and restore, handled before GTK starts:
//!
//!   omarchy-mail --backup FILE [--no-secrets]
//!   omarchy-mail --restore FILE
//!
//! The passphrase is read from OMARCHY_MAIL_BACKUP_PASSPHRASE or prompted for
//! on the terminal without echo.
use crate::backup::{self, Locations};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::Command;

const USAGE: &str =
    "Usage:\n  omarchy-mail --backup FILE [--no-secrets]\n  omarchy-mail --restore FILE";

pub fn handle(args: Vec<String>) -> Option<i32> {
    let command = args.first()?.as_str();
    if !matches!(command, "--backup" | "--restore") {
        return None;
    }
    let Some(file) = args.get(1).map(PathBuf::from) else {
        eprintln!("{USAGE}");
        return Some(2);
    };
    let include_secrets = !args.iter().any(|arg| arg == "--no-secrets");
    let result = match command {
        "--backup" => backup_command(file, include_secrets),
        _ => restore_command(file),
    };
    Some(match result {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(error) => {
            eprintln!("omarchy-mail: {error}");
            1
        }
    })
}

fn passphrase(prompt: &str) -> Result<String, String> {
    if let Ok(value) = std::env::var("OMARCHY_MAIL_BACKUP_PASSPHRASE") {
        return Ok(value);
    }
    let terminal = io::stdin().is_terminal();
    eprint!("{prompt}: ");
    let _ = io::stderr().flush();
    if terminal {
        let _ = Command::new("stty").arg("-echo").status();
    }
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);
    if terminal {
        let _ = Command::new("stty").arg("echo").status();
        eprintln!();
    }
    read.map_err(|e| e.to_string())?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn backup_command(file: PathBuf, include_secrets: bool) -> Result<String, String> {
    let secret = passphrase("Backup passphrase")?;
    if std::env::var_os("OMARCHY_MAIL_BACKUP_PASSPHRASE").is_none()
        && passphrase("Repeat passphrase")? != secret
    {
        return Err("the passphrases do not match".into());
    }
    let secrets = if include_secrets {
        let database = crate::database::Database::open_default().map_err(|e| e.to_string())?;
        backup::collect_secrets(&database, &crate::preferences::load())
    } else {
        Vec::new()
    };
    let manifest = backup::create_backup(
        &Locations::current(),
        &file,
        &secret,
        &secrets,
        include_secrets,
    )
    .map_err(|e| e.to_string())?;
    Ok(format!(
        "Backup written to {} ({} account(s), {} credential(s))",
        file.display(),
        manifest.accounts.len(),
        secrets.len()
    ))
}

fn restore_command(file: PathBuf) -> Result<String, String> {
    let secret = passphrase("Backup passphrase")?;
    let staged =
        backup::stage_restore(&Locations::current(), &file, &secret).map_err(|e| e.to_string())?;
    let keyring = backup::store_secrets(&staged.secrets)
        .err()
        .map(|error| format!("\nWarning: {error}"))
        .unwrap_or_default();
    Ok(format!(
        "Backup from {} verified ({} account(s)). Quit Omarchy Mail if it is running; the restore is applied when it next starts.{keyring}",
        staged.manifest.created_at,
        staged.manifest.accounts.len()
    ))
}
