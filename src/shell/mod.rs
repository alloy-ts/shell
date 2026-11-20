//! Bun-compatible shell runner using `fusevm` and synchronous process pipelines.

pub mod ast;
pub mod builtins;
pub mod child_process;
pub mod fusevm_bridge;
pub mod parser;
pub mod process;
pub mod repo_builtin;
pub mod vm;

pub use child_process::{
    ChildProcessOptions, ExecResult, exec, exec_file, exec_file_sync, exec_sync, spawn, spawn_sync,
};

use napi::bindgen_prelude::{Buffer, Result, Status};
use napi_derive::napi;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::shell::child_process::{
    Allowances, build_script, context, exec_result, resolve_cwd_env, sync_stdout,
};
use crate::shell::parser::{SYNTAX_ERROR_EXIT, parse_shell_script};
use crate::shell::vm::execute_pipeline_sequence;

/// Options for the `Shell` constructor and per-command `run` calls.
///
/// All allow-surfaces are deny-by-default. The JS wrapper additionally
/// defaults `allowHostExecutables` to node + npm (absolute paths) when the
/// caller does not pass one explicitly.
///
/// - `allow_host_executables`: command name -> absolute host executable path.
///   Commands not present are denied with exit code 126; builtins unaffected.
/// - `allow_local_executables`: command name -> project-relative executable
///   path (absolute also accepted), resolved against the shell cwd at run
///   time. The target is spawned directly, so it gets full host access.
/// - `allow_package_scripts`: package.json `scripts` names (or `"*"`) exposed
///   as shell commands. Script bodies are parsed and executed inside the
///   shell VM with the same allow-surfaces — they cannot escape the sandbox.
/// - `allow_node_modules_bin`: `node_modules/.bin` entry names (or `"*"`)
///   exposed as shell commands, resolved by walking up from cwd. Entries are
///   spawned directly by the OS, so each allowed name gets full host access.
#[napi(object)]
#[derive(Clone, Default)]
pub struct ShellOpts {
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub nothrow: Option<bool>,
    pub allow_host_executables: Option<HashMap<String, String>>,
    pub allow_local_executables: Option<HashMap<String, String>>,
    pub allow_package_scripts: Option<Vec<String>>,
    pub allow_node_modules_bin: Option<Vec<String>>,
}

/// Reject any non-absolute executable path in an allowlist.
fn validate_allow_host_executables(
    map: &HashMap<String, String>,
) -> std::result::Result<(), String> {
    for (name, path) in map {
        if !Path::new(path).is_absolute() {
            return Err(format!(
                "allowHostExecutables[\"{name}\"] must be an absolute path, got \"{path}\""
            ));
        }
    }
    Ok(())
}

#[napi]
pub struct Shell {
    config: Arc<Mutex<ShellOpts>>,
}

#[napi]
impl Shell {
    #[napi(constructor)]
    pub fn new(opts: Option<ShellOpts>) -> napi::Result<Self> {
        if let Some(o) = &opts {
            if let Some(map) = &o.allow_host_executables {
                validate_allow_host_executables(map)
                    .map_err(|e| napi::Error::new(Status::GenericFailure, e))?;
            }
        }
        Ok(Self {
            config: Arc::new(Mutex::new(opts.unwrap_or_default())),
        })
    }

    #[napi]
    pub fn cwd(&self, path: Option<String>) -> Option<String> {
        let mut cfg = self.config.lock().unwrap();
        if let Some(p) = path {
            cfg.cwd = Some(p.clone());
            Some(p)
        } else {
            cfg.cwd.clone()
        }
    }

    #[napi]
    pub fn env(
        &self,
        env_vars: Option<HashMap<String, String>>,
    ) -> Option<HashMap<String, String>> {
        let mut cfg = self.config.lock().unwrap();
        if let Some(ev) = env_vars {
            cfg.env = Some(ev.clone());
            Some(ev)
        } else {
            cfg.env.clone()
        }
    }

    #[napi]
    pub fn nothrow(&self) {
        let mut cfg = self.config.lock().unwrap();
        cfg.nothrow = Some(true);
    }

    #[napi]
    pub fn throws(&self, throw_on_error: bool) {
        let mut cfg = self.config.lock().unwrap();
        cfg.nothrow = Some(!throw_on_error);
    }

    /// Get or replace the host-executable allowlist. Keys are command names;
    /// values MUST be absolute paths to the real executables.
    #[napi]
    pub fn allow_host_executables(
        &self,
        allowlist: Option<HashMap<String, String>>,
    ) -> napi::Result<Option<HashMap<String, String>>> {
        let mut cfg = self.config.lock().unwrap();
        if let Some(map) = allowlist {
            validate_allow_host_executables(&map)
                .map_err(|e| napi::Error::new(Status::GenericFailure, e))?;
            cfg.allow_host_executables = Some(map.clone());
            Ok(Some(map))
        } else {
            Ok(cfg.allow_host_executables.clone())
        }
    }

    /// Get or replace the local-executable allowlist. Keys are command names;
    /// values are paths relative to the shell cwd (absolute also accepted).
    #[napi]
    pub fn allow_local_executables(
        &self,
        allowlist: Option<HashMap<String, String>>,
    ) -> Option<HashMap<String, String>> {
        let mut cfg = self.config.lock().unwrap();
        if let Some(map) = allowlist {
            cfg.allow_local_executables = Some(map.clone());
        }
        cfg.allow_local_executables.clone()
    }

    /// Get or replace the package.json script allowlist. Script names (or
    /// `"*"`) are exposed as shell commands and executed inside the shell VM.
    #[napi]
    pub fn allow_package_scripts(&self, names: Option<Vec<String>>) -> Option<Vec<String>> {
        let mut cfg = self.config.lock().unwrap();
        if let Some(n) = names {
            cfg.allow_package_scripts = Some(n.clone());
        }
        cfg.allow_package_scripts.clone()
    }

    /// Get or replace the `node_modules/.bin` allowlist. Entry names (or
    /// `"*"`) are exposed as shell commands and spawned directly.
    #[napi]
    pub fn allow_node_modules_bin(&self, names: Option<Vec<String>>) -> Option<Vec<String>> {
        let mut cfg = self.config.lock().unwrap();
        if let Some(n) = names {
            cfg.allow_node_modules_bin = Some(n.clone());
        }
        cfg.allow_node_modules_bin.clone()
    }

    #[napi]
    pub fn run(
        &self,
        script: String,
        local_cfg: Option<ShellOpts>,
    ) -> Result<ShellPromiseResult> {
        let (cwd, env_map, nothrow, allowlist, local_exec, scripts, bins) = {
            let base = self.config.lock().unwrap();
            let cwd = local_cfg
                .as_ref()
                .and_then(|c| c.cwd.clone())
                .or_else(|| base.cwd.clone())
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

            let mut env_map = base
                .env
                .clone()
                .unwrap_or_else(|| std::env::vars().collect());
            if let Some(loc) = local_cfg.as_ref().and_then(|c| c.env.clone()) {
                for (k, v) in loc {
                    env_map.insert(k, v);
                }
            }

            let nothrow = local_cfg
                .as_ref()
                .and_then(|c| c.nothrow)
                .or(base.nothrow)
                .unwrap_or(false);

            let mut allowlist = base.allow_host_executables.clone().unwrap_or_default();
            if let Some(loc) = local_cfg.as_ref().and_then(|c| c.allow_host_executables.clone()) {
                validate_allow_host_executables(&loc)
                    .map_err(|e| napi::Error::new(Status::GenericFailure, e))?;
                allowlist.extend(loc);
            }

            let mut local_exec = base.allow_local_executables.clone().unwrap_or_default();
            if let Some(loc) = local_cfg.as_ref().and_then(|c| c.allow_local_executables.clone()) {
                local_exec.extend(loc);
            }

            let mut scripts = base.allow_package_scripts.clone().unwrap_or_default();
            if let Some(loc) = local_cfg.as_ref().and_then(|c| c.allow_package_scripts.clone()) {
                scripts.extend(loc);
            }

            let mut bins = base.allow_node_modules_bin.clone().unwrap_or_default();
            if let Some(loc) = local_cfg.as_ref().and_then(|c| c.allow_node_modules_bin.clone()) {
                bins.extend(loc);
            }

            (cwd, env_map, nothrow, allowlist, local_exec, scripts, bins)
        };

        // A syntax error is a failed command, not an API error: report it the
        // same way as any other non-zero exit so `nothrow` keeps working.
        let output = match parse_shell_script(&script) {
            Ok(seq) => {
                let ctx = crate::shell::vm::ExecContext {
                    cwd: &cwd,
                    env: &env_map,
                    allow_host_executables: &allowlist,
                    allow_local_executables: &local_exec,
                    allow_package_scripts: &scripts,
                    allow_node_modules_bin: &bins,
                    depth: 0,
                };
                execute_pipeline_sequence(&seq, &ctx)
            }
            Err(e) => crate::shell::vm::ShellOutput {
                stdout: Vec::new(),
                stderr: format!("shell: syntax error: {e}\n").into_bytes(),
                exit_code: SYNTAX_ERROR_EXIT,
            },
        };

        if output.exit_code != 0 && !nothrow {
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            return Err(napi::Error::new(
                Status::GenericFailure,
                format!(
                    "Shell command failed with exit code {}: {}",
                    output.exit_code, stderr_str
                ),
            ));
        }

        Ok(ShellPromiseResult {
            stdout: Buffer::from(output.stdout),
            stderr: Buffer::from(output.stderr),
            exit_code: output.exit_code,
        })
    }

    // ---- child_process-style instance methods -------------------------
    //
    // Unlike the module-level free functions (which have no Shell instance
    // and therefore run with an empty allowlist), these inherit all four
    // allow-surfaces from `self`. `execFile`/`spawn` are quoted back into a
    // command line so they go through the very same pipeline resolution as
    // `run()` — no direct spawn, no bypass.

    fn child_run(&self, script: &str, options: Option<ChildProcessOptions>) -> Result<ExecResult> {
        let (base_cwd, base_env) = {
            let cfg = self.config.lock().unwrap();
            (cfg.cwd.clone(), cfg.env.clone())
        };
        let (cwd, env_map) = resolve_cwd_env(
            options.as_ref(),
            base_cwd.as_deref(),
            base_env.as_ref(),
        );
        let allow = {
            let cfg = self.config.lock().unwrap();
            Allowances {
                host: cfg.allow_host_executables.clone().unwrap_or_default(),
                local: cfg.allow_local_executables.clone().unwrap_or_default(),
                scripts: cfg.allow_package_scripts.clone().unwrap_or_default(),
                bins: cfg.allow_node_modules_bin.clone().unwrap_or_default(),
            }
        };
        let ctx = context(&cwd, &env_map, &allow);
        exec_result(script, &ctx)
    }

    #[napi]
    pub fn exec(&self, command: String, options: Option<ChildProcessOptions>) -> Result<ExecResult> {
        self.child_run(&command, options)
    }

    #[napi]
    pub fn exec_sync(
        &self,
        command: String,
        options: Option<ChildProcessOptions>,
    ) -> Result<Buffer> {
        let result = self.child_run(&command, options)?;
        sync_stdout(result, "execSync", &command)
    }

    #[napi]
    pub fn exec_file(
        &self,
        file: String,
        args: Option<Vec<String>>,
        options: Option<ChildProcessOptions>,
    ) -> Result<ExecResult> {
        self.child_run(&build_script(&file, args.as_ref()), options)
    }

    #[napi]
    pub fn exec_file_sync(
        &self,
        file: String,
        args: Option<Vec<String>>,
        options: Option<ChildProcessOptions>,
    ) -> Result<Buffer> {
        let script = build_script(&file, args.as_ref());
        let result = self.child_run(&script, options)?;
        sync_stdout(result, "execFileSync", &script)
    }

    #[napi]
    pub fn spawn(
        &self,
        command: String,
        args: Option<Vec<String>>,
        options: Option<ChildProcessOptions>,
    ) -> Result<ExecResult> {
        self.exec_file(command, args, options)
    }

    #[napi]
    pub fn spawn_sync(
        &self,
        command: String,
        args: Option<Vec<String>>,
        options: Option<ChildProcessOptions>,
    ) -> Result<ExecResult> {
        self.exec_file(command, args, options)
    }
}

#[napi(object)]
pub struct ShellPromiseResult {
    pub stdout: Buffer,
    pub stderr: Buffer,
    pub exit_code: i32,
}

#[napi]
pub fn braces(pattern: String) -> Vec<String> {
    let mut bridge = fusevm_bridge::ShellVMRunner::new(
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        std::env::vars().collect(),
    );
    bridge.eval_brace_expand(&pattern)
}

#[napi]
pub fn escape(s: String) -> String {
    let mut escaped = String::new();
    for c in s.chars() {
        match c {
            '$' | '`' | '"' | '\\' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_allowlist_path() {
        let mut map = HashMap::new();
        map.insert("pod".to_string(), "opt/local/usr/container".to_string());
        assert!(validate_allow_host_executables(&map).is_err());
    }

    #[test]
    fn accepts_absolute_allowlist_path() {
        let mut map = HashMap::new();
        map.insert("pod".to_string(), "/opt/local/usr/container".to_string());
        assert!(validate_allow_host_executables(&map).is_ok());
    }

    #[test]
    fn shell_constructor_rejects_relative_paths() {
        let opts = ShellOpts {
            allow_host_executables: Some(HashMap::from([(
                "pod".to_string(),
                "relative/path".to_string(),
            )])),
            ..Default::default()
        };
        assert!(Shell::new(Some(opts)).is_err());
    }

    #[test]
    fn shell_constructor_accepts_absolute_paths() {
        let opts = ShellOpts {
            allow_host_executables: Some(HashMap::from([
                ("node".to_string(), "/usr/local/bin/node".to_string()),
                ("npm".to_string(), "/usr/local/bin/npm".to_string()),
            ])),
            ..Default::default()
        };
        assert!(Shell::new(Some(opts)).is_ok());
    }

    #[test]
    fn shell_opts_default_deny_surfaces() {
        let shell = Shell::new(None).unwrap();
        assert!(shell
            .allow_package_scripts(None)
            .unwrap_or_default()
            .is_empty());
        assert!(shell
            .allow_node_modules_bin(None)
            .unwrap_or_default()
            .is_empty());
        assert!(shell
            .allow_local_executables(None)
            .unwrap_or_default()
            .is_empty());
    }

    #[test]
    fn shell_allowlist_setters_round_trip() {
        let shell = Shell::new(None).unwrap();
        shell.allow_package_scripts(Some(vec!["build".to_string(), "*".to_string()]));
        assert_eq!(
            shell.allow_package_scripts(None).unwrap(),
            vec!["build".to_string(), "*".to_string()]
        );
        shell.allow_node_modules_bin(Some(vec!["vitest".to_string()]));
        assert_eq!(
            shell.allow_node_modules_bin(None).unwrap(),
            vec!["vitest".to_string()]
        );
        shell.allow_local_executables(Some(HashMap::from([(
            "tool".to_string(),
            "scripts/tool.sh".to_string(),
        )])));
        assert_eq!(
            shell.allow_local_executables(None).unwrap().get("tool"),
            Some(&"scripts/tool.sh".to_string())
        );
    }

    #[test]
    fn shell_exec_instance_uses_allowlist() {
        let opts = ShellOpts {
            allow_host_executables: Some(HashMap::from([(
                "pod".to_string(),
                "/bin/echo".to_string(),
            )])),
            ..Default::default()
        };
        let shell = Shell::new(Some(opts)).unwrap();
        let res = shell.exec("pod via-instance".to_string(), None).unwrap();
        assert_eq!(res.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(res.stdout.as_ref()).trim(), "via-instance");
    }

    #[test]
    fn shell_exec_instance_denies_non_allowlisted() {
        let shell = Shell::new(None).unwrap();
        let res = shell
            .exec("/bin/echo escape".to_string(), None)
            .unwrap();
        assert_eq!(res.exit_code, 126);
        assert!(res.stdout.as_ref().is_empty());
    }

    #[test]
    fn shell_exec_file_quotes_args_and_uses_allowlist() {
        let shell = Shell::new(None).unwrap();
        let res = shell
            .exec_file("echo".to_string(), Some(vec!["hello world".to_string()]), None)
            .unwrap();
        assert_eq!(res.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(res.stdout.as_ref()).trim(),
            "hello world"
        );
    }

    #[test]
    fn free_exec_functions_cannot_spawn_host_executables() {
        // Module-level APIs have no Shell instance: only builtins may run.
        let res = crate::shell::child_process::exec("/bin/echo escape".to_string(), None).unwrap();
        assert_eq!(res.exit_code, 126);
        assert!(res.stdout.as_ref().is_empty());

        let res = crate::shell::child_process::exec_file(
            "echo".to_string(),
            Some(vec!["builtin-ok".to_string()]),
            None,
        )
        .unwrap();
        assert_eq!(res.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(res.stdout.as_ref()).trim(),
            "builtin-ok"
        );
    }
}
