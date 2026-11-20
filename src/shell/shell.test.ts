import { describe, expect, test } from "vite-plus/test";
import { mkdirSync, mkdtempSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  $,
  Shell,
  ShellCommandPromise,
  exec,
  execSync,
  execFile,
  execFileSync,
  spawn,
  spawnSync,
} from "./index.ts";

describe("Bun-compatible Shell ($)", () => {
  test("echo outputs text", async () => {
    const text = await $`echo "Hello World!"`.text();
    expect(text.trim()).toBe("Hello World!");
  });

  test("pwd builtin returns directory", async () => {
    const pwd = await $`pwd`.text();
    expect(pwd.trim().length).toBeGreaterThan(0);
  });

  test("builtins: false fails when nothrow is false, passes when nothrow is set", async () => {
    await expect($`false`).rejects.toThrow();

    const res = await $`false`.nothrow();
    expect(res.exitCode).toBe(1);
  });

  test("pipeline: echo | cat", async () => {
    const text = await $`echo "pipeline works" | cat`.text();
    expect(text.trim()).toBe("pipeline works");
  });

  test("output as json", async () => {
    const data = await $`echo '{"foo": "bar", "num": 42}'`.json();
    expect(data).toEqual({ foo: "bar", num: 42 });
  });

  test("output lines iterator", async () => {
    const lines: string[] = [];
    for await (const line of $`seq 1 3`.lines()) {
      lines.push(line);
    }
    expect(lines).toEqual(["1", "2", "3"]);
  });

  test("$.braces expansion via fusevm", () => {
    const expanded = $.braces("echo {1,2,3}");
    expect(expanded).toEqual(["echo 1", "echo 2", "echo 3"]);
  });

  test("$.escape logic", () => {
    const escaped = $.escape('$(foo) `bar` "baz"');
    expect(escaped).toContain("\\$(foo)");
  });

  test("environment variable override", async () => {
    const text = await $`FOO=bar echo test`.text();
    expect(text.trim()).toBe("test");
  });

  test("builtin command 'repo'", async () => {
    const help = await $`repo`.text();
    expect(help).toContain("Usage: repo <subcommand>");

    const isRepo = await $`repo is-repo`.text();
    expect(isRepo.trim()).toBe("true");

    const status = await $`repo status`.text();
    expect(status).toContain("git_dir:");
  });

  test("ShellOpts allowHostExecutables: absolute path allowlist", () => {
    const shell = new Shell({ allowHostExecutables: { pod: "/bin/echo" } });
    const res = shell.run("pod aliased");
    expect(res.stdout.toString().trim()).toBe("aliased");
  });

  test("ShellOpts allowHostExecutables: relative paths rejected", () => {
    expect(
      () =>
        new Shell({
          allowHostExecutables: { pod: "opt/local/usr/container" },
        }),
    ).toThrow(/absolute path/);
  });

  test("default Shell allows node from process.execPath", () => {
    const shell = new Shell();
    const res = shell.run(`node -e "console.log(1 + 1)"`);
    expect(res.stdout.toString().trim()).toBe("2");
  });

  test("default Shell denies non-allowlisted host executables", () => {
    const shell = new Shell();
    const res = shell.run("podman --version", { nothrow: true });
    expect(res.exitCode).toBe(126);
    expect(res.stderr.toString()).toContain("not allowed");
  });
});

describe("node:child_process alias APIs", () => {
  test("exec runs shell command", async () => {
    const res = await exec("echo child_process_exec");
    expect(res.stdout.toString().trim()).toBe("child_process_exec");
  });

  test("execSync returns stdout Buffer", () => {
    const buf = execSync("echo sync_exec");
    expect(buf.toString().trim()).toBe("sync_exec");
  });

  test("execFile executes binary", async () => {
    const res = await execFile("echo", ["file_arg"]);
    expect(res.stdout.toString().trim()).toBe("file_arg");
  });

  test("execFileSync executes binary synchronously", () => {
    const buf = execFileSync("echo", ["sync_file"]);
    expect(buf.toString().trim()).toBe("sync_file");
  });

  test("spawn / spawnSync", async () => {
    const asyncRes = await spawn("echo", ["spawned"]);
    expect(asyncRes.stdout.toString().trim()).toBe("spawned");

    const syncRes = spawnSync("echo", ["spawned_sync"]);
    expect(syncRes.stdout.toString().trim()).toBe("spawned_sync");
  });
});

describe("allowPackageScripts / allowNodeModulesBin / allowLocalExecutables", () => {
  function makeProject(
    tag: string,
    scripts: Record<string, string> = { hello: "echo pkg-script" },
  ) {
    const dir = mkdtempSync(join(tmpdir(), `shell-project-${tag}-`));
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "proj", scripts }));
    mkdirSync(join(dir, "node_modules/.bin"), { recursive: true });
    const binPath = join(dir, "node_modules/.bin/toolbin");
    writeFileSync(binPath, "#!/bin/sh\necho bin-tool\n");
    chmodSync(binPath, 0o755);
    mkdirSync(join(dir, "scripts"), { recursive: true });
    const devPath = join(dir, "scripts/dev.sh");
    writeFileSync(devPath, "#!/bin/sh\necho local-tool\n");
    chmodSync(devPath, 0o755);
    return dir;
  }

  test("package script exposed as command when allowed", async () => {
    const dir = makeProject("scripts");
    const shell = new Shell({ cwd: dir, allowPackageScripts: ["hello"] });
    const res = shell.run("hello extra");
    expect(res.exitCode).toBe(0);
    expect(res.stdout.toString().trim()).toBe("pkg-script extra");
  });

  test("package script denied by default", () => {
    const dir = makeProject("script-deny");
    const shell = new Shell({ cwd: dir });
    const res = shell.run("hello", { nothrow: true });
    expect(res.exitCode).toBe(126);
    expect(res.stdout.toString()).toBe("");
  });

  test("package script body cannot escape the allowlist", () => {
    const dir = makeProject("script-escape", { evil: "/bin/echo pwned" });
    const shell = new Shell({ cwd: dir, allowPackageScripts: ["evil"] });
    const res = shell.run("evil", { nothrow: true });
    // Script bodies run inside the shell VM, so /bin/echo is still denied.
    expect(res.exitCode).toBe(126);
    expect(res.stdout.toString()).toBe("");
  });

  test("node_modules/.bin executable runs when allowed", () => {
    const dir = makeProject("bin");
    const shell = new Shell({ cwd: dir, allowNodeModulesBin: ["toolbin"] });
    const res = shell.run("toolbin");
    expect(res.exitCode).toBe(0);
    expect(res.stdout.toString().trim()).toBe("bin-tool");
  });

  test("node_modules/.bin denied by default", () => {
    const dir = makeProject("bin-deny");
    const shell = new Shell({ cwd: dir });
    const res = shell.run("toolbin", { nothrow: true });
    expect(res.exitCode).toBe(126);
    expect(res.stdout.toString()).toBe("");
  });

  test("local executable runs when allowed via project-relative path", () => {
    const dir = makeProject("local");
    const shell = new Shell({
      cwd: dir,
      allowLocalExecutables: { dev: "scripts/dev.sh" },
    });
    const res = shell.run("dev");
    expect(res.exitCode).toBe(0);
    expect(res.stdout.toString().trim()).toBe("local-tool");
  });

  test("local executable with missing target reports 127", () => {
    const dir = makeProject("local-missing");
    const shell = new Shell({
      cwd: dir,
      allowLocalExecutables: { dev: "scripts/nope.sh" },
    });
    const res = shell.run("dev", { nothrow: true });
    expect(res.exitCode).toBe(127);
    expect(res.stderr.toString()).toContain("not found");
  });

  test("fluent allowNodeModulesBin on a command promise", async () => {
    const dir = makeProject("fluent");
    const shell = new Shell({ cwd: dir });
    const res = await new ShellCommandPromise(shell, "toolbin").allowNodeModulesBin([
      "toolbin",
    ]);
    expect(res.exitCode).toBe(0);
    expect(res.stdout.toString().trim()).toBe("bin-tool");
  });
});
