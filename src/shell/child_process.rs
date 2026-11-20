//! Node:child_process compatible alias APIs.
//!
//! Every entry point is routed through the shell VM: the module-level free
//! functions use an empty allowlist (builtins only), while `Shell`'s instance
//! methods inherit the shell's four allow-surfaces. Nothing here spawns an
//! arbitrary executable outside the sandbox.

use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::shell::parser::{SYNTAX_ERROR_EXIT, parse_shell_script};
use crate::shell::vm::{ExecContext, execute_pipeline_sequence};

/// Options for the child_process-style APIs.
///
/// `shell`, `timeout`, `max_buffer` and `encoding` are accepted for API
/// compatibility but ignored: execution always goes through the VM, which
/// never shells out to an arbitrary interpreter.
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

/// The four allow-surfaces handed to the VM. Empty by default for the
/// module-level free functions; `Shell` instance methods populate them from
/// their own configuration.
#[derive(Default)]
pub struct Allowances {
    pub host: HashMap<String, String>,
    pub local: HashMap<String, String>,
    pub scripts: Vec<String>,
    pub bins: Vec<String>,
}

/// Resolve cwd/env from options, falling back to the shell's base config.
pub fn resolve_cwd_env(
    options: Option<&ChildProcessOptions>,
    base_cwd: Option<&str>,
    base_env: Option<&HashMap<String, String>>,
) -> (PathBuf, HashMap<String, String>) {
    let cwd = options
        .and_then(|o| o.cwd.clone())
        .or_else(|| base_cwd.map(str::to_string))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let mut env = base_env.cloned().unwrap_or_else(|| std::env::vars().collect());
    if let Some(opts) = options {
        if let Some(overrides) = &opts.env {
            for (k, v) in overrides {
                env.insert(k.clone(), v.clone());
            }
        }
    }
    (cwd, env)
}

/// Build an execution context borrowing the resolved cwd/env/allowances.
pub fn context<'a>(
    cwd: &'a Path,
    env: &'a HashMap<String, String>,
    allow: &'a Allowances,
) -> ExecContext<'a> {
    ExecContext {
        cwd,
        env,
        allow_host_executables: &allow.host,
        allow_local_executables: &allow.local,
        allow_package_scripts: &allow.scripts,
        allow_node_modules_bin: &allow.bins,
        depth: 0,
    }
}

/// Shell-quote a single argument for the VM parser (single quotes with
/// backslash escape for embedded quotes, which the parser handles).
pub fn quote_arg(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '=' | ',' | '+')
        })
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Turn an `execFile`/`spawn` invocation into a shell command line so it goes
/// through exactly the same allowlist resolution as `Shell.run`.
pub fn build_script(file: &str, args: Option<&Vec<String>>) -> String {
    let mut script = quote_arg(file);
    if let Some(args) = args {
        for arg in args {
            script.push(' ');
            script.push_str(&quote_arg(arg));
        }
    }
    script
}

/// Parse and execute a script in the given context, returning the
/// child_process-style result.
///
/// A syntax error is reported as a failed command (exit code 2) rather than
/// an API error, matching how `exec` reports a non-zero exit.
pub fn exec_result(script: &str, ctx: &ExecContext) -> Result<ExecResult> {
    let out = match parse_shell_script(script) {
        Ok(seq) => execute_pipeline_sequence(&seq, ctx),
        Err(e) => crate::shell::vm::ShellOutput {
            stdout: Vec::new(),
            stderr: format!("shell: syntax error: {e}\n").into_bytes(),
            exit_code: SYNTAX_ERROR_EXIT,
        },
    };
    Ok(ExecResult {
        stdout: Buffer::from(out.stdout),
        stderr: Buffer::from(out.stderr),
        exit_code: out.exit_code,
        status: Some(out.exit_code),
    })
}

/// Convert a non-zero result into the error shape of the `*Sync` APIs.
pub fn sync_stdout(result: ExecResult, label: &str, script: &str) -> Result<Buffer> {
    if result.exit_code != 0 {
        let msg = String::from_utf8_lossy(result.stderr.as_ref());
        return Err(napi::Error::new(
            Status::GenericFailure,
            format!("{label} failed with exit code {}: {}{}", result.exit_code, script, msg),
        ));
    }
    Ok(result.stdout)
}

/// Module-level free functions have no Shell instance, so they run with an
/// empty allowlist — only builtins are available.
fn free_exec(script: &str, options: Option<ChildProcessOptions>) -> Result<ExecResult> {
    let (cwd, env) = resolve_cwd_env(options.as_ref(), None, None);
    let allow = Allowances::default();
    let ctx = context(&cwd, &env, &allow);
    exec_result(script, &ctx)
}

#[napi]
pub fn exec(command: String, options: Option<ChildProcessOptions>) -> Result<ExecResult> {
    free_exec(&command, options)
}

#[napi]
pub fn exec_sync(command: String, options: Option<ChildProcessOptions>) -> Result<Buffer> {
    let result = free_exec(&command, options)?;
    sync_stdout(result, "execSync", &command)
}

#[napi]
pub fn exec_file(
    file: String,
    args: Option<Vec<String>>,
    options: Option<ChildProcessOptions>,
) -> Result<ExecResult> {
    free_exec(&build_script(&file, args.as_ref()), options)
}

#[napi]
pub fn exec_file_sync(
    file: String,
    args: Option<Vec<String>>,
    options: Option<ChildProcessOptions>,
) -> Result<Buffer> {
    let script = build_script(&file, args.as_ref());
    let result = free_exec(&script, options)?;
    sync_stdout(result, "execFileSync", &script)
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
    exec_file(command, args, options)
}
