//! Node:child_process compatible alias APIs:
//! exec, execSync, execFile, execFileSync, spawn, spawnSync

use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::shell::parser::parse_shell_script;
use crate::shell::process::{CommandOptions, EnvEntry, outcome_from_command_result, run_command};
use crate::shell::vm::execute_pipeline_sequence;

#[napi(object)]
pub struct ChildProcessOptions {
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub timeout: Option<f64>,
    pub max_buffer: Option<f64>,
    pub encoding: Option<String>,
    pub shell: Option<String>,
}

#[napi(object)]
pub struct ExecResult {
    pub stdout: Buffer,
    pub stderr: Buffer,
    pub exit_code: i32,
    pub status: Option<i32>,
}

#[napi]
pub fn exec(command: String, options: Option<ChildProcessOptions>) -> Result<ExecResult> {
    let opts = options.unwrap_or(ChildProcessOptions {
        cwd: None,
        env: None,
        timeout: None,
        max_buffer: None,
        encoding: None,
        shell: None,
    });

    let cwd = opts
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let env_map = opts.env.unwrap_or_else(|| std::env::vars().collect());

    let seq =
        parse_shell_script(&command).map_err(|e| napi::Error::new(Status::GenericFailure, e))?;
    // The standalone exec API has no ShellOpts, so no host executables are
    // allowlisted here: only shell builtins can run through the VM pipeline.
    // Use execFile/spawn for direct, unrestricted process execution.
    let ctx = crate::shell::vm::ExecContext {
        cwd: &cwd,
        env: &env_map,
        allow_host_executables: &HashMap::new(),
        allow_local_executables: &HashMap::new(),
        allow_package_scripts: &[],
        allow_node_modules_bin: &[],
        depth: 0,
    };
    let out = execute_pipeline_sequence(&seq, &ctx);

    Ok(ExecResult {
        stdout: Buffer::from(out.stdout),
        stderr: Buffer::from(out.stderr),
        exit_code: out.exit_code,
        status: Some(out.exit_code),
    })
}

#[napi]
pub fn exec_sync(command: String, options: Option<ChildProcessOptions>) -> Result<Buffer> {
    let opts = options.unwrap_or(ChildProcessOptions {
        cwd: None,
        env: None,
        timeout: None,
        max_buffer: None,
        encoding: None,
        shell: None,
    });

    let cwd = opts
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let shell_cmd = if cfg!(windows) { "cmd" } else { "sh" };
    let shell_arg = if cfg!(windows) { "/c" } else { "-c" };

    let mut cmd = Command::new(opts.shell.as_deref().unwrap_or(shell_cmd));
    cmd.arg(shell_arg).arg(&command);
    cmd.current_dir(cwd);

    if let Some(env) = opts.env {
        cmd.envs(env);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd
        .output()
        .map_err(|e| napi::Error::new(Status::GenericFailure, format!("execSync failed: {e}")))?;

    if !output.status.success() {
        return Err(napi::Error::new(
            Status::GenericFailure,
            format!(
                "Command failed: {}\n{}",
                command,
                String::from_utf8_lossy(&output.stderr)
            ),
        ));
    }

    Ok(Buffer::from(output.stdout))
}

#[napi]
pub fn exec_file(
    file: String,
    args: Option<Vec<String>>,
    options: Option<ChildProcessOptions>,
) -> Result<ExecResult> {
    let opts = options.unwrap_or(ChildProcessOptions {
        cwd: None,
        env: None,
        timeout: None,
        max_buffer: None,
        encoding: None,
        shell: None,
    });

    let env_entries = opts.env.map(|map| {
        map.into_iter()
            .map(|(name, value)| EnvEntry { name, value })
            .collect()
    });

    let cmd_opts = CommandOptions {
        program: file,
        args,
        cwd: opts.cwd,
        env: env_entries,
        input: None,
        timeout_ms: opts.timeout,
        cleanup_timeout_ms: Some(5000.0),
        limits: None,
    };

    let result = run_command(cmd_opts).map_err(|e| napi::Error::new(Status::GenericFailure, e))?;
    let (stdout, stderr, exit_code) = outcome_from_command_result(result);

    Ok(ExecResult {
        stdout: Buffer::from(stdout),
        stderr: Buffer::from(stderr),
        exit_code,
        status: Some(exit_code),
    })
}

#[napi]
pub fn exec_file_sync(
    file: String,
    args: Option<Vec<String>>,
    options: Option<ChildProcessOptions>,
) -> Result<Buffer> {
    let opts = options.unwrap_or(ChildProcessOptions {
        cwd: None,
        env: None,
        timeout: None,
        max_buffer: None,
        encoding: None,
        shell: None,
    });

    let mut cmd = Command::new(&file);
    if let Some(arguments) = args {
        cmd.args(arguments);
    }
    if let Some(cwd) = opts.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = opts.env {
        cmd.envs(env);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd.output().map_err(|e| {
        napi::Error::new(Status::GenericFailure, format!("execFileSync failed: {e}"))
    })?;

    if !output.status.success() {
        return Err(napi::Error::new(
            Status::GenericFailure,
            format!(
                "Command failed: {}\n{}",
                file,
                String::from_utf8_lossy(&output.stderr)
            ),
        ));
    }

    Ok(Buffer::from(output.stdout))
}

#[napi]
pub fn spawn(
    command: String,
    args: Option<Vec<String>>,
    options: Option<ChildProcessOptions>,
) -> Result<ExecResult> {
    exec_file(command, args, options)
}

#[napi]
pub fn spawn_sync(
    command: String,
    args: Option<Vec<String>>,
    options: Option<ChildProcessOptions>,
) -> Result<ExecResult> {
    let opts = options.unwrap_or(ChildProcessOptions {
        cwd: None,
        env: None,
        timeout: None,
        max_buffer: None,
        encoding: None,
        shell: None,
    });

    let mut cmd = Command::new(&command);
    if let Some(arguments) = args {
        cmd.args(arguments);
    }
    if let Some(cwd) = opts.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = opts.env {
        cmd.envs(env);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd
        .output()
        .map_err(|e| napi::Error::new(Status::GenericFailure, format!("spawnSync failed: {e}")))?;

    let exit_code = output.status.code().unwrap_or(1);
    Ok(ExecResult {
        stdout: Buffer::from(output.stdout),
        stderr: Buffer::from(output.stderr),
        exit_code,
        status: Some(exit_code),
    })
}
