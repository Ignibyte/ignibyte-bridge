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

    println!("agent-bridge doctor");
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
    if resolved_program.contains("/opt/homebrew/bin/claude") {
        warnings.push(
            "/opt/homebrew/bin/claude matched; this older Claude binary previously emitted zero PTY bytes in project directories."
                .to_string(),
        );
    }
    if command_is_claude(&command_parts[0]) && !resolved_program.contains("/.local/bin/claude") {
        warnings.push("known-good local Claude path was ~/.local/bin/claude in this environment; resolved path differs.".to_string());
    }
    if !executable_found {
        warnings.push("resolved program is not executable or could not be found.".to_string());
    }

    if command_is_claude(&command_parts[0]) || command_is_claude(&resolved_program) {
        println!();
        println!("claude_version_via_agent_bridge_path:");
        print_command_output(
            Command::new(&resolved_program)
                .arg("--version")
                .current_dir(&cwd)
                .env("PATH", &child_path)
                .output(),
        );

        println!();
        println!("claude_version_via_login_shell:");
        print_command_output(
            Command::new("zsh")
                .args(["-lic", "command -v claude; claude --version"])
                .current_dir(&cwd)
                .output(),
        );
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
