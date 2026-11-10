use napi_derive::napi;

#[path = "../sources/shell/src/mod.rs"]
pub mod shell;
pub use shell::{
    ChildProcessOptions, ExecResult, Shell, ShellOpts, ShellPromiseResult, braces, escape, exec,
    exec_file, exec_file_sync, exec_sync, spawn, spawn_sync,
};

#[napi]
pub fn add(left: u32, right: u32) -> u32 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
