//! Shell execution engine using `fusevm` VM and ShellHost, managing process pipelines and builtins.

use fusevm::host::ShellHost;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::shell::ast::{
    CommandNode, LogicalOp, PipelineNode, PipelineSequence, RedirectOp, Redirection,
};
use crate::shell::builtins::{is_builtin, run_builtin};
use crate::shell::parser::{SYNTAX_ERROR_EXIT, parse_shell_script};
use crate::shell::process::{CommandOptions, EnvEntry, run_command};

/// Maximum recursion depth when package scripts invoke other package scripts.
/// Guards against `"scripts": { "a": "npm run a" }`-style self-referential loops.
pub const MAX_SCRIPT_DEPTH: usize = 8;

/// Execution context threaded through the whole pipeline evaluation.
///
/// Four allow-surfaces exist, all deny-by-default (except host executables,
/// which the JS wrapper defaults to node + npm):
///
/// - `allow_host_executables`: command name -> absolute host executable path.
/// - `allow_local_executables`: command name -> path relative to the shell cwd
///   (absolute paths also accepted), resolved at run time. The target file
///   must exist; it is spawned directly, so it gets full host access.
/// - `allow_package_scripts`: package.json `scripts` names (or `"*"`) exposed
///   as commands. Script bodies are parsed and executed **inside this shell
///   VM** with the same allow-surfaces — they can never escape the sandbox.
/// - `allow_node_modules_bin`: `node_modules/.bin` entry names (or `"*"`)
///   exposed as commands. Resolved by walking up from cwd; spawned directly
///   by the OS, so each allowed entry gets full host access.
#[derive(Clone, Copy)]
pub struct ExecContext<'a> {
    pub cwd: &'a Path,
    pub env: &'a HashMap<String, String>,
    pub allow_host_executables: &'a HashMap<String, String>,
    pub allow_local_executables: &'a HashMap<String, String>,
    pub allow_package_scripts: &'a [String],
    pub allow_node_modules_bin: &'a [String],
    /// Nesting level of package-script execution (0 = top level).
    pub depth: usize,
}

pub struct ShellExecutionHost {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
}

impl ShellExecutionHost {
    pub fn new(cwd: PathBuf, env: HashMap<String, String>) -> Self {
        Self { cwd, env }
    }
}

impl ShellHost for ShellExecutionHost {
    fn brace_expand(&mut self, s: &str) -> Vec<String> {
        expand_braces(s)
    }

    fn tilde_expand(&mut self, s: &str) -> String {
        if s == "~" || s.starts_with("~/") {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            s.replacen('~', &home, 1)
        } else {
            s.to_string()
        }
    }
}

pub fn expand_braces(input: &str) -> Vec<String> {
    if let Some(open) = input.find('{') {
        if let Some(close) = input[open..].find('}').map(|idx| open + idx) {
            let prefix = &input[..open];
            let suffix = &input[close + 1..];
            let inner = &input[open + 1..close];

            // Check if it's numeric range like {1..5}
            if let Some(dots) = inner.find("..") {
                let start_str = &inner[..dots];
                let end_str = &inner[dots + 2..];
                if let (Ok(start), Ok(end)) = (start_str.parse::<i64>(), end_str.parse::<i64>()) {
                    let mut res = Vec::new();
                    if start <= end {
                        for i in start..=end {
                            let expanded = format!("{}{}{}", prefix, i, suffix);
                            res.extend(expand_braces(&expanded));
                        }
                    } else {
                        for i in (end..=start).rev() {
                            let expanded = format!("{}{}{}", prefix, i, suffix);
                            res.extend(expand_braces(&expanded));
                        }
                    }
                    return res;
                }
            }

            // Comma-separated list {a,b,c}
            let parts: Vec<&str> = inner.split(',').collect();
            let mut res = Vec::new();
            for part in parts {
                let expanded = format!("{}{}{}", prefix, part, suffix);
                res.extend(expand_braces(&expanded));
            }
            return res;
        }
    }
    vec![input.to_string()]
}

pub struct ShellOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

/// Exit code and stderr produced when a command is not permitted by any
/// allow-surface. 126 mirrors the shell convention for "command found but not
/// executable".
fn redir_path(cwd: &Path, target: &str) -> std::result::Result<PathBuf, ShellOutput> {
    crate::shell::builtins::confined_path(cwd, target).map_err(|e| ShellOutput {
        stdout: Vec::new(),
        stderr: format!("shell: {e}\n").into_bytes(),
        exit_code: 1,
    })
}

fn denied_output(command: &str) -> ShellOutput {
    ShellOutput {
        stdout: Vec::new(),
        stderr: format!(
            "shell: {command}: host executable not allowed (missing from allowHostExecutables; \
             also consider allowLocalExecutables, allowPackageScripts or allowNodeModulesBin)\n"
        )
        .into_bytes(),
        exit_code: 126,
    }
}

/// Walk up from `start` looking for the first existing `rel` path.
fn find_up(start: &Path, rel: &Path) -> Option<PathBuf> {
    let mut dir = Some(start.to_path_buf());
    while let Some(d) = dir {
        let candidate = d.join(rel);
        if candidate.exists() {
            return Some(candidate);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

/// Read a script body from the nearest package.json walking up from cwd.
fn read_package_script(cwd: &Path, name: &str) -> Option<String> {
    let pkg_path = find_up(cwd, Path::new("package.json"))?;
    let content = fs::read_to_string(pkg_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value
        .get("scripts")?
        .get(name)?
        .as_str()
        .map(str::to_string)
}

/// Whether an allow list (script names or .bin names) permits `name`.
/// The entry `"*"` permits everything.
fn allow_list_permits(list: &[String], name: &str) -> bool {
    list.iter().any(|s| s == "*" || s == name)
}

/// Resolve a `node_modules/.bin` entry by walking up from cwd.
fn resolve_node_modules_bin(cwd: &Path, name: &str) -> Option<PathBuf> {
    let bin_dir = find_up(cwd, Path::new("node_modules").join(".bin").as_path())?;
    let mut candidates = vec![name.to_string()];
    if cfg!(windows) {
        candidates.push(format!("{name}.cmd"));
        candidates.push(format!("{name}.exe"));
    }
    for candidate in candidates {
        let p = bin_dir.join(&candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Resolve an entry of `allow_local_executables` against the shell cwd.
fn resolve_local_executable(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Execute a package.json script by parsing its body and running it inside
/// this shell VM with the same allow-surfaces (recursion-depth guarded).
fn run_package_script(name: &str, args: &[String], ctx: &ExecContext) -> ShellOutput {
    if ctx.depth >= MAX_SCRIPT_DEPTH {
        return ShellOutput {
            stdout: Vec::new(),
            stderr: format!(
                "shell: {name}: package script nesting too deep (limit {MAX_SCRIPT_DEPTH})\n"
            )
            .into_bytes(),
            exit_code: 1,
        };
    }
    let Some(body) = read_package_script(ctx.cwd, name) else {
        return denied_output(name);
    };
    // npm-style argument forwarding: `greet a b` runs the body with " a b" appended.
    let script = if args.is_empty() {
        body
    } else {
        format!("{body} {}", args.join(" "))
    };
    let seq = match parse_shell_script(&script) {
        Ok(seq) => seq,
        Err(e) => {
            return ShellOutput {
                stdout: Vec::new(),
                stderr: format!("shell: {name}: failed to parse package script: {e}\n")
                    .into_bytes(),
                exit_code: SYNTAX_ERROR_EXIT,
            };
        }
    };
    let nested = ExecContext {
        depth: ctx.depth + 1,
        ..*ctx
    };
    execute_pipeline_sequence(&seq, &nested)
}

pub fn execute_pipeline_sequence(sequence: &PipelineSequence, ctx: &ExecContext) -> ShellOutput {
    execute_pipeline_sequence_with_input(sequence, ctx, &[])
}

pub fn execute_pipeline_sequence_with_input(
    sequence: &PipelineSequence,
    ctx: &ExecContext,
    input: &[u8],
) -> ShellOutput {
    let mut last_stdout = Vec::new();
    let mut last_stderr = Vec::new();
    let mut last_exit_code = 0;

    for (op, pipeline) in &sequence.pipelines {
        match op {
            LogicalOp::And => {
                if last_exit_code != 0 {
                    continue;
                }
            }
            LogicalOp::Or => {
                if last_exit_code == 0 {
                    continue;
                }
            }
            LogicalOp::Sequential => {}
        }

        let output = execute_pipeline(pipeline, ctx, input);
        last_exit_code = output.exit_code;
        // A shell concatenates the output of every command in a sequence —
        // only the last *stage of a pipeline* produces that pipeline's stdout.
        last_stdout.extend_from_slice(&output.stdout);
        last_stderr.extend_from_slice(&output.stderr);
    }

    ShellOutput {
        stdout: last_stdout,
        stderr: last_stderr,
        exit_code: last_exit_code,
    }
}

fn execute_pipeline(pipeline: &PipelineNode, ctx: &ExecContext, input: &[u8]) -> ShellOutput {
    let mut current_stdin: Vec<u8> = input.to_vec();
    let mut last_stdout = Vec::new();
    let mut last_stderr = Vec::new();
    let mut last_exit = 0;

    for (i, cmd) in pipeline.commands.iter().enumerate() {
        let is_last = i == pipeline.commands.len() - 1;
        let input = std::mem::take(&mut current_stdin);
        let res = execute_command(cmd, input, ctx);
        last_exit = res.exit_code;
        if is_last {
            last_stdout = res.stdout;
            last_stderr.extend_from_slice(&res.stderr);
        } else {
            current_stdin = res.stdout;
            // Intermediary stderr is still collected or forwarded
            last_stderr.extend_from_slice(&res.stderr);
        }
    }

    ShellOutput {
        stdout: last_stdout,
        stderr: last_stderr,
        exit_code: last_exit,
    }
}

fn execute_command(cmd: &CommandNode, mut stdin_data: Vec<u8>, ctx: &ExecContext) -> ShellOutput {
    let cwd = ctx.cwd;
    let env_vars = ctx.env;
    if cmd.args.is_empty() {
        if cmd.redirects.is_empty() {
            return ShellOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 0,
            };
        }
        // Redirection-only command (e.g. `> out.txt`): the redirections still
        // apply, exactly like in a POSIX shell.
        let (stdout, stderr) = match apply_redirects(cmd, cwd, Vec::new(), Vec::new()) {
            Ok(pair) => pair,
            Err(out) => return out,
        };
        return ShellOutput {
            stdout,
            stderr,
            exit_code: 0,
        };
    }

    // Merge env
    let mut effective_env = env_vars.clone();
    for (k, v) in &cmd.env_vars {
        effective_env.insert(k.clone(), v.clone());
    }

    // Process input redirections `<`
    for redir in &cmd.redirects {
        if let Redirection::File {
            op: RedirectOp::Input,
            target,
        } = redir
        {
            let file_path = match redir_path(cwd, target) {
                Ok(p) => p,
                Err(out) => return out,
            };
            match fs::read(&file_path) {
                Ok(bytes) => stdin_data = bytes,
                Err(e) => {
                    return ShellOutput {
                        stdout: Vec::new(),
                        stderr: format!("{}: {}\n", file_path.display(), e).into_bytes(),
                        exit_code: 1,
                    };
                }
            }
        }
    }

    let prog_name = &cmd.args[0];
    let args = &cmd.args[1..];

    // Resolution order:
    //   1. shell builtins (always permitted)
    //   2. allowHostExecutables (absolute host paths)
    //   3. allowLocalExecutables (project-relative paths, resolved at run time)
    //   4. allowPackageScripts / allowNodeModulesBin — script names take
    //      precedence at the top level (they are exposed as commands), while
    //      inside a package script body the .bin lookup comes first to match
    //      `npm run` semantics where .bin is prepended to PATH.
    // Anything else is denied with exit 126.
    fn resolve_non_builtin(
        prog_name: &str,
        args: &[String],
        stdin_data: &[u8],
        effective_env: &HashMap<String, String>,
        ctx: &ExecContext,
    ) -> Option<(Vec<u8>, Vec<u8>, i32)> {
        if let Some(executable) = ctx.allow_host_executables.get(prog_name) {
            return Some(run_external(
                executable,
                args,
                stdin_data,
                ctx.cwd,
                effective_env,
            ));
        }
        if let Some(rel_path) = ctx.allow_local_executables.get(prog_name) {
            let resolved = resolve_local_executable(ctx.cwd, rel_path);
            if !resolved.is_file() {
                return Some((
                    Vec::new(),
                    format!(
                        "shell: {prog_name}: local executable not found at {}\n",
                        resolved.display()
                    )
                    .into_bytes(),
                    127,
                ));
            }
            return Some(run_external(
                &resolved.to_string_lossy(),
                args,
                stdin_data,
                ctx.cwd,
                effective_env,
            ));
        }
        let script_first =
            ctx.depth == 0 || !allow_list_permits(ctx.allow_node_modules_bin, prog_name);
        if script_first {
            if allow_list_permits(ctx.allow_package_scripts, prog_name)
                && read_package_script(ctx.cwd, prog_name).is_some()
            {
                let out = run_package_script(prog_name, args, ctx);
                return Some((out.stdout, out.stderr, out.exit_code));
            }
            if allow_list_permits(ctx.allow_node_modules_bin, prog_name) {
                if let Some(bin_path) = resolve_node_modules_bin(ctx.cwd, prog_name) {
                    return Some(run_external(
                        &bin_path.to_string_lossy(),
                        args,
                        stdin_data,
                        ctx.cwd,
                        effective_env,
                    ));
                }
            }
        } else {
            if let Some(bin_path) = resolve_node_modules_bin(ctx.cwd, prog_name) {
                return Some(run_external(
                    &bin_path.to_string_lossy(),
                    args,
                    stdin_data,
                    ctx.cwd,
                    effective_env,
                ));
            }
            if allow_list_permits(ctx.allow_package_scripts, prog_name)
                && read_package_script(ctx.cwd, prog_name).is_some()
            {
                let out = run_package_script(prog_name, args, ctx);
                return Some((out.stdout, out.stderr, out.exit_code));
            }
        }
        None
    }

    let (stdout, stderr, exit_code) = if is_builtin(prog_name) {
        let git_executable = ctx.allow_host_executables.get("git").map(String::as_str);
        if let Some(res) = run_builtin(prog_name, args, &stdin_data, cwd, git_executable) {
            (res.stdout, res.stderr, res.exit_code)
        } else {
            resolve_non_builtin(prog_name, args, &stdin_data, &effective_env, ctx).unwrap_or_else(
                || {
                    let d = denied_output(prog_name);
                    (d.stdout, d.stderr, d.exit_code)
                },
            )
        }
    } else {
        resolve_non_builtin(prog_name, args, &stdin_data, &effective_env, ctx).unwrap_or_else(
            || {
                let d = denied_output(prog_name);
                (d.stdout, d.stderr, d.exit_code)
            },
        )
    };

    // Process output redirections
    let (stdout, stderr) = match apply_redirects(cmd, cwd, stdout, stderr) {
        Ok(pair) => pair,
        Err(out) => return out,
    };

    ShellOutput {
        stdout,
        stderr,
        exit_code,
    }
}

/// Apply a command's output redirections to the captured stdout/stderr.
///
/// Returns `Err(output)` when a redirection target escapes the shell cwd.
fn apply_redirects(
    cmd: &CommandNode,
    cwd: &Path,
    mut stdout: Vec<u8>,
    mut stderr: Vec<u8>,
) -> Result<(Vec<u8>, Vec<u8>), ShellOutput> {
    for redir in &cmd.redirects {
        match redir {
            Redirection::File {
                op: RedirectOp::OutputTruncate,
                target,
            } => {
                let file_path = redir_path(cwd, target)?;
                let _ = fs::write(file_path, &stdout);
                stdout.clear();
            }
            Redirection::File {
                op: RedirectOp::OutputAppend,
                target,
            } => {
                let file_path = redir_path(cwd, target)?;
                if let Ok(mut f) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(file_path)
                {
                    use std::io::Write;
                    let _ = f.write_all(&stdout);
                }
                stdout.clear();
            }
            Redirection::File {
                op: RedirectOp::StderrTruncate,
                target,
            } => {
                let file_path = redir_path(cwd, target)?;
                let _ = fs::write(file_path, &stderr);
                stderr.clear();
            }
            Redirection::File {
                op: RedirectOp::StderrAppend,
                target,
            } => {
                let file_path = redir_path(cwd, target)?;
                if let Ok(mut f) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(file_path)
                {
                    use std::io::Write;
                    let _ = f.write_all(&stderr);
                }
                stderr.clear();
            }
            Redirection::File {
                op: RedirectOp::BothTruncate,
                target,
            } => {
                let file_path = redir_path(cwd, target)?;
                let mut combined = stdout.clone();
                combined.extend_from_slice(&stderr);
                let _ = fs::write(file_path, combined);
                stdout.clear();
                stderr.clear();
            }
            Redirection::File {
                op: RedirectOp::BothAppend,
                target,
            } => {
                let file_path = redir_path(cwd, target)?;
                if let Ok(mut f) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(file_path)
                {
                    use std::io::Write;
                    let _ = f.write_all(&stdout);
                    let _ = f.write_all(&stderr);
                }
                stdout.clear();
                stderr.clear();
            }
            Redirection::StderrToStdout => {
                stdout.extend_from_slice(&stderr);
                stderr.clear();
            }
            Redirection::StdoutToStderr => {
                stderr.extend_from_slice(&stdout);
                stdout.clear();
            }
            Redirection::File {
                op: RedirectOp::Input,
                ..
            } => {}
        }
    }

    Ok((stdout, stderr))
}

fn run_external(
    program: &str,
    args: &[String],
    stdin_data: &[u8],
    cwd: &Path,
    env_vars: &HashMap<String, String>,
) -> (Vec<u8>, Vec<u8>, i32) {
    let env_entries = env_vars
        .iter()
        .map(|(k, v)| EnvEntry {
            name: k.clone(),
            value: v.clone(),
        })
        .collect();

    let options = CommandOptions {
        program: program.to_string(),
        args: Some(args.to_vec()),
        cwd: Some(cwd.to_string_lossy().to_string()),
        env: Some(env_entries),
        input: if stdin_data.is_empty() {
            None
        } else {
            Some(stdin_data.to_vec())
        },
        timeout_ms: None,
        cleanup_timeout_ms: Some(5000.0),
        limits: None,
    };

    match run_command(options) {
        Ok(cmd_result) => crate::shell::process::outcome_from_command_result(cmd_result),
        Err(e) => (Vec::new(), format!("{}\n", e).into_bytes(), 127),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::parser::parse_shell_script;

    #[cfg(unix)]
    #[test]
    fn supported_literal_commands_match_dash() {
        if std::process::Command::new("dash")
            .arg("-c")
            .arg(":")
            .output()
            .is_err()
        {
            eprintln!("dash unavailable; skipping POSIX reference comparison");
            return;
        }
        let dir = temp_dir("posix-subset");
        let ctx = TestCtx::new(&dir);
        for script in [
            "echo hello",
            "echo 'a b'",
            "echo one | cat",
            "false || echo yes",
            "true && echo yes",
            "echo foo#bar # comment",
            "echo a; echo b",
            "echo hi | # comment\n cat",
        ] {
            let reference = std::process::Command::new("dash")
                .arg("-c")
                .arg(script)
                .current_dir(&dir)
                .output()
                .expect("dash is required for the POSIX comparison test");
            let actual = ctx.run(script);
            assert_eq!(actual.stdout, reference.stdout, "stdout: {script}");
            assert_eq!(actual.stderr, reference.stderr, "stderr: {script}");
            assert_eq!(
                actual.exit_code,
                reference.status.code().unwrap(),
                "status: {script}"
            );
        }
    }

    struct TestCtx {
        cwd: PathBuf,
        allow_host: HashMap<String, String>,
        allow_local: HashMap<String, String>,
        allow_scripts: Vec<String>,
        allow_bin: Vec<String>,
    }

    impl TestCtx {
        fn new(cwd: &Path) -> Self {
            Self {
                cwd: cwd.to_path_buf(),
                allow_host: HashMap::new(),
                allow_local: HashMap::new(),
                allow_scripts: Vec::new(),
                allow_bin: Vec::new(),
            }
        }

        fn run(&self, script: &str) -> ShellOutput {
            let seq = parse_shell_script(script).unwrap();
            let ctx = ExecContext {
                cwd: &self.cwd,
                env: &std::env::vars().collect(),
                allow_host_executables: &self.allow_host,
                allow_local_executables: &self.allow_local,
                allow_package_scripts: &self.allow_scripts,
                allow_node_modules_bin: &self.allow_bin,
                depth: 0,
            };
            execute_pipeline_sequence(&seq, &ctx)
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("shell-vm-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn write_exec(path: &Path, body: &str) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        drop(f);
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn allowlisted_absolute_path_runs() {
        let mut ctx = TestCtx::new(Path::new("."));
        ctx.allow_host
            .insert("pod".to_string(), "/bin/echo".to_string());
        let out = ctx.run("pod aliased");
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "aliased");
    }

    #[test]
    fn non_allowlisted_command_denied() {
        let ctx = TestCtx::new(Path::new("."));
        let out = ctx.run("pod aliased");
        assert_eq!(out.exit_code, 126);
        assert!(String::from_utf8_lossy(&out.stderr).contains("not allowed"));
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn builtins_run_without_allowlist() {
        let ctx = TestCtx::new(Path::new("."));
        let out = ctx.run("echo hello");
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }

    #[test]
    fn allowlist_maps_command_name_to_executable() {
        let mut ctx = TestCtx::new(Path::new("."));
        // Command written as `greet` actually spawns /bin/echo.
        ctx.allow_host
            .insert("greet".to_string(), "/bin/echo".to_string());
        let out = ctx.run("greet mapped");
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "mapped");
    }

    #[test]
    fn denied_command_short_circuits_logical_and() {
        let ctx = TestCtx::new(Path::new("."));
        let out = ctx.run("pod x && echo after");
        assert_eq!(out.exit_code, 126);
        // `echo after` must not run because the denied command failed.
        assert!(String::from_utf8_lossy(&out.stdout).is_empty());
    }

    #[test]
    fn package_script_runs_in_vm_with_args() {
        let dir = temp_dir("script");
        fs::write(
            dir.join("package.json"),
            r#"{"name":"t","scripts":{"greet":"echo script-body"}}"#,
        )
        .unwrap();
        let mut ctx = TestCtx::new(&dir);
        ctx.allow_scripts = vec!["greet".to_string()];
        let out = ctx.run("greet extra");
        assert_eq!(
            out.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "script-body extra"
        );
    }

    #[test]
    fn package_script_body_cannot_escape_allowlist() {
        let dir = temp_dir("script-escape");
        // The script body tries to spawn a host command that is NOT allowlisted.
        // Because scripts run inside the VM, this must still be denied (126).
        fs::write(
            dir.join("package.json"),
            r#"{"name":"t","scripts":{"evil":"/bin/echo pwned"}}"#,
        )
        .unwrap();
        let mut ctx = TestCtx::new(&dir);
        ctx.allow_scripts = vec!["evil".to_string()];
        let out = ctx.run("evil");
        assert_eq!(out.exit_code, 126);
        assert!(String::from_utf8_lossy(&out.stdout).is_empty());
    }

    #[test]
    fn package_script_not_in_allowlist_denied() {
        let dir = temp_dir("script-deny");
        fs::write(
            dir.join("package.json"),
            r#"{"name":"t","scripts":{"greet":"echo hi"}}"#,
        )
        .unwrap();
        let ctx = TestCtx::new(&dir);
        let out = ctx.run("greet");
        assert_eq!(out.exit_code, 126);
    }

    #[test]
    fn package_script_can_call_allowed_bin() {
        let dir = temp_dir("script-bin");
        #[cfg(unix)]
        write_exec(
            &dir.join("node_modules/.bin/hello"),
            "#!/bin/sh\necho bin-hello\n",
        );
        fs::write(
            dir.join("package.json"),
            r#"{"name":"t","scripts":{"all":"echo start && hello"}}"#,
        )
        .unwrap();
        let mut ctx = TestCtx::new(&dir);
        ctx.allow_scripts = vec!["all".to_string()];
        ctx.allow_bin = vec!["hello".to_string()];
        let out = ctx.run("all");
        assert_eq!(
            out.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The script body is `echo start && hello`; both commands contribute
        // to the output, and the .bin tool's output proves it ran inside the
        // script.
        assert!(String::from_utf8_lossy(&out.stdout).contains("start"));
        assert!(String::from_utf8_lossy(&out.stdout).contains("bin-hello"));
    }

    #[test]
    fn package_script_recursion_is_depth_limited() {
        let dir = temp_dir("script-loop");
        fs::write(
            dir.join("package.json"),
            r#"{"name":"t","scripts":{"self":"self"}}"#,
        )
        .unwrap();
        let mut ctx = TestCtx::new(&dir);
        ctx.allow_scripts = vec!["self".to_string()];
        let out = ctx.run("self");
        // Must terminate with an error instead of hanging.
        assert_ne!(out.exit_code, 0);
    }

    #[test]
    fn node_modules_bin_runs_when_allowed() {
        let dir = temp_dir("bin");
        #[cfg(unix)]
        write_exec(
            &dir.join("node_modules/.bin/mytool"),
            "#!/bin/sh\necho bin-tool\n",
        );
        let mut ctx = TestCtx::new(&dir);
        ctx.allow_bin = vec!["mytool".to_string()];
        let out = ctx.run("mytool");
        assert_eq!(
            out.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "bin-tool");
    }

    #[test]
    fn node_modules_bin_denied_by_default() {
        let dir = temp_dir("bin-deny");
        #[cfg(unix)]
        write_exec(
            &dir.join("node_modules/.bin/mytool"),
            "#!/bin/sh\necho bin-tool\n",
        );
        let ctx = TestCtx::new(&dir);
        let out = ctx.run("mytool");
        assert_eq!(out.exit_code, 126);
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn node_modules_bin_wildcard_allows() {
        let dir = temp_dir("bin-star");
        #[cfg(unix)]
        write_exec(
            &dir.join("node_modules/.bin/anything"),
            "#!/bin/sh\necho wildcard-tool\n",
        );
        let mut ctx = TestCtx::new(&dir);
        ctx.allow_bin = vec!["*".to_string()];
        let out = ctx.run("anything");
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "wildcard-tool");
    }

    #[test]
    fn node_modules_bin_resolved_from_nested_cwd() {
        let dir = temp_dir("bin-nested");
        #[cfg(unix)]
        write_exec(
            &dir.join("node_modules/.bin/upstream"),
            "#!/bin/sh\necho from-root\n",
        );
        let nested = dir.join("packages/app/src");
        fs::create_dir_all(&nested).unwrap();
        let mut ctx = TestCtx::new(&nested);
        ctx.allow_bin = vec!["upstream".to_string()];
        let out = ctx.run("upstream");
        assert_eq!(
            out.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "from-root");
    }

    #[test]
    fn local_executable_runs_when_allowed() {
        let dir = temp_dir("local");
        #[cfg(unix)]
        write_exec(&dir.join("scripts/dev.sh"), "#!/bin/sh\necho local-tool\n");
        let mut ctx = TestCtx::new(&dir);
        ctx.allow_local
            .insert("tool".to_string(), "scripts/dev.sh".to_string());
        let out = ctx.run("tool");
        assert_eq!(
            out.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "local-tool");
    }

    #[test]
    fn local_executable_missing_file_reports_127() {
        let dir = temp_dir("local-missing");
        let mut ctx = TestCtx::new(&dir);
        ctx.allow_local
            .insert("tool".to_string(), "scripts/nope.sh".to_string());
        let out = ctx.run("tool");
        assert_eq!(out.exit_code, 127);
        assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
    }

    #[test]
    fn local_executable_not_in_allowlist_denied() {
        let dir = temp_dir("local-deny");
        #[cfg(unix)]
        write_exec(&dir.join("scripts/dev.sh"), "#!/bin/sh\necho local-tool\n");
        let ctx = TestCtx::new(&dir);
        let out = ctx.run("tool");
        assert_eq!(out.exit_code, 126);
        assert!(out.stdout.is_empty());
    }

    // --- Output handling across command sequences ---

    #[test]
    fn sequence_concatenates_stdout() {
        let ctx = TestCtx::new(Path::new("."));
        let out = ctx.run("echo one && echo two");
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "one\ntwo\n");
    }

    #[test]
    fn newline_separated_commands_both_run() {
        let ctx = TestCtx::new(Path::new("."));
        let out = ctx.run("echo one\necho two");
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "one\ntwo\n");
    }

    #[test]
    fn short_circuited_command_contributes_no_output() {
        let ctx = TestCtx::new(Path::new("."));
        let out = ctx.run("pod x && echo after");
        assert_eq!(out.exit_code, 126);
        assert!(String::from_utf8_lossy(&out.stdout).is_empty());
    }

    #[test]
    fn both_redirect_captures_stdout() {
        let dir = temp_dir("both-redirect");
        let ctx = TestCtx::new(&dir);
        let out = ctx.run("echo captured &> out.txt");
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.is_empty(), "stdout: {:?}", out.stdout);
        let written = fs::read_to_string(dir.join("out.txt")).unwrap();
        assert_eq!(written, "captured\n");
    }

    #[test]
    fn both_append_redirect_appends() {
        let dir = temp_dir("both-append");
        let ctx = TestCtx::new(&dir);
        assert_eq!(ctx.run("echo one &>> out.txt").exit_code, 0);
        assert_eq!(ctx.run("echo two &>> out.txt").exit_code, 0);
        let written = fs::read_to_string(dir.join("out.txt")).unwrap();
        assert_eq!(written, "one\ntwo\n");
    }

    #[test]
    fn redirection_only_command_creates_file() {
        let dir = temp_dir("redirect-only");
        let ctx = TestCtx::new(&dir);
        let out = ctx.run("> created.txt");
        assert_eq!(out.exit_code, 0);
        assert!(dir.join("created.txt").exists());
    }

    #[test]
    fn syntax_error_is_not_executed() {
        let dir = temp_dir("syntax");
        let ctx = TestCtx::new(&dir);
        let seq = parse_shell_script("echo one & echo two");
        assert!(seq.is_err(), "background operator must be a syntax error");
    }
}
