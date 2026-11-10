//! `repo` builtin implemented with the `git` CLI so it works without the
//! monorepo's gix-backed repository module.

use crate::shell::builtins::BuiltinResult;
use std::path::Path;
use std::process::Command;

fn run_git(cwd: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("git").args(args).current_dir(cwd).output()
}

fn ok(stdout: Vec<u8>) -> BuiltinResult {
    BuiltinResult {
        exit_code: 0,
        stdout,
        stderr: Vec::new(),
    }
}

fn fail(message: String) -> BuiltinResult {
    BuiltinResult {
        exit_code: 1,
        stdout: Vec::new(),
        stderr: message.into_bytes(),
    }
}

pub fn builtin_repo(args: &[String], cwd: &Path) -> BuiltinResult {
    if args.is_empty() {
        let help = b"Usage: repo <subcommand> [args...]\n\
Subcommands:\n  \
status           Show status of repository at cwd\n  \
init [path]      Initialize a new repository\n  \
head             Show current HEAD commit ID\n  \
is-repo [path]   Check if path is a git repository\n  \
is-bare          Check if repository is bare\n";
        return ok(help.to_vec());
    }

    let subcmd = args[0].as_str();
    let subargs = &args[1..];

    match subcmd {
        "status" => match run_git(cwd, &["rev-parse", "--absolute-git-dir"]) {
            Ok(out) if out.status.success() => {
                let git_dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let mut text = format!("git_dir: {}\n", git_dir);

                if let Ok(out) = run_git(cwd, &["rev-parse", "--show-toplevel"]) {
                    if out.status.success() {
                        let work_dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        text.push_str(&format!("work_dir: {}\n", work_dir));
                    }
                }
                if let Ok(out) = run_git(cwd, &["status", "--porcelain"]) {
                    let is_empty = out.stdout.is_empty();
                    text.push_str(&format!("is_empty: {}\n", is_empty));
                }
                if let Ok(out) = run_git(cwd, &["rev-parse", "HEAD"]) {
                    if out.status.success() {
                        let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        text.push_str(&format!("head: {}\n", head));
                    }
                }

                ok(text.into_bytes())
            }
            Ok(out) => fail(format!(
                "repo status error: {}\n",
                String::from_utf8_lossy(&out.stderr)
            )),
            Err(e) => fail(format!("repo status error: {}\n", e)),
        },
        "init" => {
            let target = subargs
                .first()
                .map(|p| cwd.join(p))
                .unwrap_or_else(|| cwd.to_path_buf());
            let target_str = target.to_string_lossy().to_string();

            match Command::new("git")
                .args(["init", "--initial-branch=main"])
                .arg(&target_str)
                .current_dir(cwd)
                .output()
            {
                Ok(out) if out.status.success() => ok(
                    format!("Initialized repository at {}\n", target_str).into_bytes(),
                ),
                Ok(out) => fail(format!(
                    "repo init error: {}\n",
                    String::from_utf8_lossy(&out.stderr)
                )),
                Err(e) => fail(format!("repo init error: {}\n", e)),
            }
        }
        "head" => match run_git(cwd, &["rev-parse", "HEAD"]) {
            Ok(out) if out.status.success() => {
                ok(format!("{}\n", String::from_utf8_lossy(&out.stdout).trim()).into_bytes())
            }
            Ok(out) => fail(format!(
                "repo head error: {}\n",
                String::from_utf8_lossy(&out.stderr)
            )),
            Err(e) => fail(format!("repo head error: {}\n", e)),
        },
        "is-repo" => {
            let target = subargs
                .first()
                .map(|p| cwd.join(p))
                .unwrap_or_else(|| cwd.to_path_buf());
            let result = Command::new("git")
                .args(["rev-parse", "--is-inside-work-tree"])
                .current_dir(target)
                .output();
            let is_repo = matches!(result, Ok(ref out) if out.status.success());
            ok(format!("{}\n", is_repo).into_bytes())
        }
        "is-bare" => match run_git(cwd, &["rev-parse", "--is-bare-repository"]) {
            Ok(out) if out.status.success() => {
                ok(format!("{}\n", String::from_utf8_lossy(&out.stdout).trim()).into_bytes())
            }
            Ok(out) => fail(format!(
                "repo error: {}\n",
                String::from_utf8_lossy(&out.stderr)
            )),
            Err(e) => fail(format!("repo error: {}\n", e)),
        },
        unknown => fail(format!("Unknown subcommand 'repo {}'\n", unknown)),
    }
}
