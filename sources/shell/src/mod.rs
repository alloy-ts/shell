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

use crate::shell::parser::parse_shell_script;
use crate::shell::vm::execute_pipeline_sequence;

/// Options for the `Shell` constructor and per-command `run` calls.
///
/// `allow_host_executables` is the allowlist of host executables the shell may
/// spawn. Keys are command names as written in a script; values MUST be
/// absolute paths to the real executable (e.g. `/opt/local/usr/container` or
/// `process.execPath` for node). Commands not present in the map are denied
/// with exit code 126; shell builtins are unaffected.
#[napi(object)]
#[derive(Clone, Default)]
pub struct ShellOpts {
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub nothrow: Option<bool>,
    pub allow_host_executables: Option<HashMap<String, String>>,
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

    #[napi]
    pub fn run(
        &self,
        script: String,
        local_cfg: Option<ShellOpts>,
    ) -> Result<ShellPromiseResult> {
        let (cwd, env_map, nothrow, allowlist) = {
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

            (cwd, env_map, nothrow, allowlist)
        };

        let seq =
            parse_shell_script(&script).map_err(|e| napi::Error::new(Status::GenericFailure, e))?;

        let output = execute_pipeline_sequence(&seq, &cwd, &env_map, &allowlist);

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
}
