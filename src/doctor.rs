//! Environment and command-resolution diagnostics.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context, Result};

use crate::paths::{
    command_is_claude, is_executable, parse_command, path_with_local_bin, resolve_program_path,
    sessions_root,
};

pub fn doctor(cmd: &str, cwd: Option<PathBuf>) -> Result<()> {
    let cwd = cwd
        .unwrap_or(std::env::current_dir().context("failed to read current directory")?)
        .canonicalize()
        .context("failed to canonicalize cwd")?;
    if !cwd.is_dir() {
        bail!("cwd is not a directory: {}", cwd.display());
    }

    let command_parts = parse_command(cmd)?;
    let child_path =
        path_with_local_bin().unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
    let resolved_program = resolve_program_path(&command_parts[0], &child_path)
        .unwrap_or_else(|| command_parts[0].clone());
    let executable_found = is_executable(Path::new(&resolved_program));

    println!("ignibyte-bridge doctor");
    println!("sessions_root: {}", sessions_root()?.display());
    println!("cwd: {}", cwd.display());
    println!("input_command: {cmd}");
    println!("parsed_command: {}", command_parts.join(" "));
    println!("resolved_program: {resolved_program}");
    println!("executable_found: {executable_found}");
    println!("child_path:");
    for (index, entry) in child_path
        .split(':')
        .filter(|entry| !entry.is_empty())
        .enumerate()
    {
        println!("  {:>2}: {entry}", index + 1);
    }

    let mut warnings = Vec::new();
    if !executable_found {
        warnings.push("resolved program is not executable or could not be found.".to_string());
    }

    // For Claude specifically, derive warnings from observed behavior rather
    // than hardcoded install paths: does the resolved binary actually report a
    // version, and does the login shell resolve `claude` to the same binary
    // Ignibyte Bridge would spawn?
    if command_is_claude(&command_parts[0]) || command_is_claude(&resolved_program) {
        println!();
        println!("claude_version_via_ignibyte_bridge_path:");
        let bridge_version = Command::new(&resolved_program)
            .arg("--version")
            .current_dir(&cwd)
            .env("PATH", &child_path)
            .output();
        let bridge_ok = matches!(&bridge_version, Ok(output) if output.status.success());
        print_command_output(bridge_version);

        // Compare against the user's actual login shell, not a hardcoded one.
        // A unique sentinel before `command -v` makes the path robust to dotfile
        // banners that print to stdout on login.
        let login_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        const SENTINEL: &str = "__IGNIBYTE_BRIDGE_CLAUDE__";
        println!();
        println!("claude_resolution_via_login_shell ({login_shell}):");
        let login = Command::new(&login_shell)
            .args([
                "-lic",
                &format!("printf '{SENTINEL}\\n'; command -v claude"),
            ])
            .current_dir(&cwd)
            .output();
        // Take the first absolute path on the line after the sentinel.
        let login_path = login.as_ref().ok().and_then(|output| {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut lines = text.lines();
            lines.find(|line| line.contains(SENTINEL))?;
            lines
                .map(str::trim)
                .find(|line| line.starts_with('/'))
                .map(str::to_string)
        });
        print_command_output(login);

        if !bridge_ok {
            warnings.push(
                "`claude --version` via the resolved path did not succeed; the session may fail to start."
                    .to_string(),
            );
        }
        match login_path {
            Some(login_path) if login_path != resolved_program => warnings.push(format!(
                "login shell resolves claude to '{login_path}', but Ignibyte Bridge resolves '{resolved_program}'; sessions may run a different binary than your terminal."
            )),
            // No path found (claude not on the login shell's PATH, or the shell
            // was unavailable) — that is not necessarily a problem, so stay quiet.
            _ => {}
        }
    }

    println!();
    if warnings.is_empty() {
        println!("warnings: none");
    } else {
        println!("warnings:");
        for warning in warnings {
            println!("  - {warning}");
        }
    }

    Ok(())
}

fn print_command_output(output: std::io::Result<std::process::Output>) {
    match output {
        Ok(output) => {
            println!("  status: {}", output.status);
            print_output_block("stdout", &output.stdout);
            print_output_block("stderr", &output.stderr);
        }
        Err(error) => println!("  failed: {error}"),
    }
}

fn print_output_block(label: &str, bytes: &[u8]) {
    let contents = String::from_utf8_lossy(bytes);
    let contents = contents.trim_end();
    if contents.is_empty() {
        println!("  {label}: <empty>");
    } else {
        println!("  {label}:");
        for line in contents.lines() {
            println!("    {line}");
        }
    }
}
