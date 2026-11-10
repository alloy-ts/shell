//! Shell execution engine using `fusevm` VM and ShellHost, managing process pipelines and builtins.

use fusevm::host::ShellHost;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::shell::ast::{CommandNode, LogicalOp, PipelineNode, PipelineSequence, RedirectOp};
use crate::shell::builtins::{is_builtin, run_builtin};
use crate::shell::parser::parse_shell_script;
use crate::shell::process::{CommandOptions, EnvEntry, outcome_from_command_result, run_command};

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

/// Resolve a command name through the configured executable aliases.
///
/// Alias values are either a literal path or a `$(command)` expression. The
/// latter is evaluated via `run_command` with the shell's cwd/env and its
/// trimmed stdout becomes the executable. On failure the original name is
/// returned so a broken alias cannot silently change the command.
pub fn resolve_executable_alias(
    program: &str,
    aliases: &HashMap<String, String>,
    cwd: &Path,
    env: &HashMap<String, String>,
) -> String {
    let Some(value) = aliases.get(program) else {
        return program.to_string();
    };
    let value = value.trim();
    if value.is_empty() {
        return program.to_string();
    }
    if let Some(inner) = value.strip_prefix("$(").and_then(|s| s.strip_suffix(')')) {
        return eval_command_substitution(inner, cwd, env).unwrap_or_else(|| program.to_string());
    }
    value.to_string()
}

fn eval_command_substitution(
    command: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Option<String> {
    let seq = parse_shell_script(command).ok()?;
    let args = &seq.pipelines.first()?.1.commands.first()?.args;
    let program = args.first()?.clone();
    let cmd_args = args[1..].to_vec();
    if program.is_empty() {
        return None;
    }

    let env_entries = env
        .iter()
        .map(|(name, value)| EnvEntry {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();

    let result = run_command(CommandOptions {
        program,
        args: Some(cmd_args),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        env: Some(env_entries),
        input: None,
        timeout_ms: None,
        cleanup_timeout_ms: Some(5000.0),
        limits: None,
    })
    .ok()?;

    let (stdout, _stderr, exit_code) = outcome_from_command_result(result);
    if exit_code != 0 {
        return None;
    }
    let resolved = String::from_utf8_lossy(&stdout).trim().to_string();
    if resolved.is_empty() {
        None
    } else {
        Some(resolved)
    }
}

pub fn execute_pipeline_sequence(
    sequence: &PipelineSequence,
    cwd: &Path,
    env_vars: &HashMap<String, String>,
    aliases: &HashMap<String, String>,
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

        let output = execute_pipeline(pipeline, cwd, env_vars, aliases);
        last_exit_code = output.exit_code;
        last_stdout = output.stdout;
        last_stderr = output.stderr;
    }

    ShellOutput {
        stdout: last_stdout,
        stderr: last_stderr,
        exit_code: last_exit_code,
    }
}

fn execute_pipeline(
    pipeline: &PipelineNode,
    cwd: &Path,
    env_vars: &HashMap<String, String>,
    aliases: &HashMap<String, String>,
) -> ShellOutput {
    let mut current_stdin: Vec<u8> = Vec::new();
    let mut last_stdout = Vec::new();
    let mut last_stderr = Vec::new();
    let mut last_exit = 0;

    for (i, cmd) in pipeline.commands.iter().enumerate() {
        let is_last = i == pipeline.commands.len() - 1;
        let input = std::mem::take(&mut current_stdin);
        let res = execute_command(cmd, input, cwd, env_vars, aliases);
        last_exit = res.exit_code;
        if is_last {
            last_stdout = res.stdout;
            last_stderr = res.stderr;
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

fn execute_command(
    cmd: &CommandNode,
    mut stdin_data: Vec<u8>,
    cwd: &Path,
    env_vars: &HashMap<String, String>,
    aliases: &HashMap<String, String>,
) -> ShellOutput {
    if cmd.args.is_empty() {
        return ShellOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
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
        if matches!(redir.op, RedirectOp::Input) {
            let file_path = cwd.join(&redir.target);
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

    let resolved = resolve_executable_alias(&cmd.args[0], aliases, cwd, &effective_env);
    let prog_name = resolved.as_str();
    let args = &cmd.args[1..];

    // Check builtins first
    let (mut stdout, mut stderr, exit_code) = if is_builtin(prog_name) {
        if let Some(res) = run_builtin(prog_name, args, &stdin_data, cwd) {
            (res.stdout, res.stderr, res.exit_code)
        } else {
            run_external(prog_name, args, &stdin_data, cwd, &effective_env)
        }
    } else {
        run_external(prog_name, args, &stdin_data, cwd, &effective_env)
    };

    // Process output redirections
    for redir in &cmd.redirects {
        match redir.op {
            RedirectOp::OutputTruncate => {
                let file_path = cwd.join(&redir.target);
                let _ = fs::write(file_path, &stdout);
                stdout.clear();
            }
            RedirectOp::OutputAppend => {
                let file_path = cwd.join(&redir.target);
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
            RedirectOp::StderrTruncate => {
                let file_path = cwd.join(&redir.target);
                let _ = fs::write(file_path, &stderr);
                stderr.clear();
            }
            RedirectOp::StderrAppend => {
                let file_path = cwd.join(&redir.target);
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
            RedirectOp::BothTruncate => {
                let file_path = cwd.join(&redir.target);
                let mut combined = stdout.clone();
                combined.extend_from_slice(&stderr);
                let _ = fs::write(file_path, combined);
                stdout.clear();
                stderr.clear();
            }
            RedirectOp::BothAppend => {
                let file_path = cwd.join(&redir.target);
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
            RedirectOp::StderrToStdout => {
                stdout.extend_from_slice(&stderr);
                stderr.clear();
            }
            RedirectOp::StdoutToStderr => {
                stderr.extend_from_slice(&stdout);
                stdout.clear();
            }
            RedirectOp::Input => {}
        }
    }

    ShellOutput {
        stdout,
        stderr,
        exit_code,
    }
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

    fn env() -> HashMap<String, String> {
        std::env::vars().collect()
    }

    #[test]
    fn alias_literal_path() {
        let mut aliases = HashMap::new();
        aliases.insert("pod".to_string(), "opt/local/usr/container".to_string());
        let resolved = resolve_executable_alias("pod", &aliases, Path::new("."), &env());
        assert_eq!(resolved, "opt/local/usr/container");
    }

    #[test]
    fn alias_command_substitution() {
        let mut aliases = HashMap::new();
        aliases.insert("pod".to_string(), "$(echo opt/local/usr/container)".to_string());
        let resolved = resolve_executable_alias("pod", &aliases, Path::new("."), &env());
        assert_eq!(resolved, "opt/local/usr/container");
    }

    #[test]
    fn alias_missing_command_falls_back() {
        let aliases = HashMap::new();
        let resolved = resolve_executable_alias("pod", &aliases, Path::new("."), &env());
        assert_eq!(resolved, "pod");
    }

    #[test]
    fn alias_broken_substitution_falls_back() {
        let mut aliases = HashMap::new();
        aliases.insert(
            "pod".to_string(),
            "$(definitely_not_a_real_command_xyz)".to_string(),
        );
        let resolved = resolve_executable_alias("pod", &aliases, Path::new("."), &env());
        assert_eq!(resolved, "pod");
    }

    #[test]
    fn alias_applies_in_pipeline() {
        let mut aliases = HashMap::new();
        aliases.insert("pod".to_string(), "$(echo /bin/echo)".to_string());
        let seq = parse_shell_script("pod aliased").unwrap();
        let out = execute_pipeline_sequence(&seq, Path::new("."), &env(), &aliases);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "aliased");
    }
}
