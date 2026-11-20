//! Synchronous subprocess runner shared by the shell VM and the napi Task.
//!
//! All types here are plain data so the runner stays independent of the napi
//! binding layer. Timeout and resource-limit options are accepted for API
//! compatibility but are not enforced by this minimal runner.

use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct EnvEntry {
    pub name: String,
    pub value: String,
}

#[derive(Debug)]
pub struct CommandOptions {
    pub program: String,
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub env: Option<Vec<EnvEntry>>,
    pub input: Option<Vec<u8>>,
    pub timeout_ms: Option<f64>,
    pub cleanup_timeout_ms: Option<f64>,
    pub limits: Option<()>,
}

#[derive(Debug)]
pub struct ProcessOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

#[derive(Debug)]
pub struct ProcessStatus {
    pub code: Option<i32>,
}

#[derive(Debug)]
pub struct ProcessFailure {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: Option<ProcessStatus>,
}

#[derive(Debug)]
pub struct CommandResult {
    pub output: Option<ProcessOutput>,
    pub failure: Option<ProcessFailure>,
}

/// Spawn a child, feed optional stdin, and capture stdout/stderr.
///
/// Returns `Err(message)` only for spawn/write/wait I/O failures. A non-zero
/// exit status is a `failure` in the returned `CommandResult`, not an error.
pub fn run_command(options: CommandOptions) -> std::result::Result<CommandResult, String> {
    let mut cmd = Command::new(&options.program);
    if let Some(args) = &options.args {
        cmd.args(args);
    }
    if let Some(cwd) = &options.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = &options.env {
        for entry in env {
            cmd.env(&entry.name, &entry.value);
        }
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if options.input.is_some() {
        cmd.stdin(Stdio::piped());
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {}", options.program, e))?;

    if let (Some(input), Some(stdin)) = (options.input.as_ref(), child.stdin.take()) {
        let mut stdin = stdin;
        stdin
            .write_all(input)
            .map_err(|e| format!("failed to write stdin for {}: {}", options.program, e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for {}: {}", options.program, e))?;

    let code = output.status.code();
    if output.status.success() {
        Ok(CommandResult {
            output: Some(ProcessOutput {
                stdout: output.stdout,
                stderr: output.stderr,
                exit_code: code.unwrap_or(0),
            }),
            failure: None,
        })
    } else {
        Ok(CommandResult {
            output: None,
            failure: Some(ProcessFailure {
                stdout: output.stdout,
                stderr: output.stderr,
                status: Some(ProcessStatus { code }),
            }),
        })
    }
}

/// Flatten a `CommandResult` into `(stdout, stderr, exit_code)` plain data.
pub fn outcome_from_command_result(result: CommandResult) -> (Vec<u8>, Vec<u8>, i32) {
    if let Some(out) = result.output {
        (out.stdout, out.stderr, out.exit_code)
    } else if let Some(fail) = result.failure {
        let code = fail.status.and_then(|s| s.code).unwrap_or(1);
        (fail.stdout, fail.stderr, code)
    } else {
        (Vec::new(), Vec::new(), 0)
    }
}
