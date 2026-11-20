import { describe, expect, test } from "vite-plus/test";
import { execFileSync } from "node:child_process";
import { $, Shell } from "./index.ts";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

describe("Shell Builtin Commands", () => {
  test("echo: normal and -n flag", async () => {
    const normal = await $`echo hello world`.text();
    expect(normal).toBe("hello world\n");

    const noNewline = await $`echo -n hello`.text();
    expect(noNewline).toBe("hello");
  });

  test("pwd", async () => {
    const cwd = await $`pwd`.text();
    expect(cwd.trim()).toBe(process.cwd());
  });

  test("seq: range and step", async () => {
    const seq1 = await $`seq 3`.text();
    expect(seq1.trim().split("\n")).toEqual(["1", "2", "3"]);

    const seq2 = await $`seq 1 2 5`.text();
    expect(seq2.trim().split("\n")).toEqual(["1", "3", "5"]);
  });

  test("dirname and basename", async () => {
    const dir = await $`dirname /path/to/file.txt`.text();
    expect(dir.trim()).toBe("/path/to");

    const base = await $`basename /path/to/file.txt`.text();
    expect(base.trim()).toBe("file.txt");

    const baseSuffix = await $`basename /path/to/file.txt .txt`.text();
    expect(baseSuffix.trim()).toBe("file");
  });

  test("true and false exit codes", async () => {
    const resTrue = await $`true`.nothrow();
    expect(resTrue.exitCode).toBe(0);

    const resFalse = await $`false`.nothrow();
    expect(resFalse.exitCode).toBe(1);
  });

  test("wc counts lines, words, bytes from stdin or files", async () => {
    const wcStdin = await $`echo "hello world" | wc -w`.text();
    expect(wcStdin.trim()).toBe("2");

    const wcLines = await $`seq 1 5 | wc -l`.text();
    expect(wcLines.trim()).toBe("5");
  });

  test("grep filters lines with case insensitivity and invert match", async () => {
    const matched = await $`echo "apple\nBanana\ncherry" | grep -i "banana"`.text();
    expect(matched.trim()).toBe("Banana");

    const inverted = await $`echo "apple\nBanana\ncherry" | grep -v "a"`.text();
    expect(inverted.trim()).toBe("cherry");
  });

  test("touch, ls, cat, and rm", () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "shell-test-"));
    const shell = new Shell({ cwd: tmpDir });

    // touch
    expect(shell.run("touch test.txt").exitCode).toBe(0);
    expect(fs.existsSync(path.join(tmpDir, "test.txt"))).toBe(true);

    // echo > file (redirections are confined to cwd too)
    expect(shell.run('echo "contents" > test.txt').exitCode).toBe(0);

    // cat
    expect(shell.exec("cat test.txt").stdout.toString().trim()).toBe("contents");

    // ls
    expect(shell.exec("ls").stdout.toString().trim()).toContain("test.txt");

    // rm
    expect(shell.run("rm test.txt").exitCode).toBe(0);
    expect(fs.existsSync(path.join(tmpDir, "test.txt"))).toBe(false);

    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  test("mkdir and mv", () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "shell-mkdir-"));
    const shell = new Shell({ cwd: tmpDir });

    expect(shell.run("mkdir -p sub/sub2").exitCode).toBe(0);
    expect(fs.existsSync(path.join(tmpDir, "sub/sub2"))).toBe(true);

    expect(shell.run("touch foo.txt").exitCode).toBe(0);
    expect(shell.run("mv foo.txt bar.txt").exitCode).toBe(0);
    expect(fs.existsSync(path.join(tmpDir, "foo.txt"))).toBe(false);
    expect(fs.existsSync(path.join(tmpDir, "bar.txt"))).toBe(true);

    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  test("filesystem access is confined to shell cwd", () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "shell-confine-"));
    const shell = new Shell({ cwd: tmpDir });
    const outside = path.join(tmpDir, "..", "shell-escape-probe");

    const absolute = shell.run(`touch ${outside}`, { nothrow: true });
    expect(absolute.exitCode).toBe(1);
    expect(absolute.stderr.toString()).toContain("outside shell cwd");

    const traversal = shell.run("touch ../shell-escape-probe", { nothrow: true });
    expect(traversal.exitCode).toBe(1);
    expect(traversal.stderr.toString()).toContain("outside shell cwd");

    const redirect = shell.run("echo hi > ../shell-escape-probe", { nothrow: true });
    expect(redirect.exitCode).toBe(1);
    expect(redirect.stderr.toString()).toContain("outside shell cwd");

    expect(fs.existsSync(outside)).toBe(false);
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  test("which finds system executables", async () => {
    const nodePath = await $`which node`.text();
    expect(nodePath.trim().length).toBeGreaterThan(0);
  });

  test("awk builtin field extraction, -F separator, NR and NF", async () => {
    const basic = await $`echo "first second third" | awk '{print $1, $3}'`.text();
    expect(basic).toBe("first third\n");

    const customDelim = await $`echo "foo:bar:baz" | awk -F: '{print $2}'`.text();
    expect(customDelim).toBe("bar\n");

    const nrAndWhole = await $`echo "alpha\nbeta" | awk '{print NR, $0}'`.text();
    expect(nrAndWhole).toBe("1 alpha\n2 beta\n");

    const lastField = await $`echo "one two three four" | awk '{print NF, $NF}'`.text();
    expect(lastField).toBe("4 four\n");
  });

  test("repo and git builtin require an allowlisted git", () => {
    const denied = new Shell().exec("git status");
    expect(denied.exitCode).toBe(126);
    expect(denied.stderr.toString()).toContain("not allowed");

    const gitPath = execFileSync("which", ["git"], { encoding: "utf8" }).trim();
    const shell = new Shell({ allowHostExecutables: { git: gitPath } });

    expect(shell.exec("repo").stdout.toString()).toContain("Usage: repo <subcommand>");
    expect(shell.exec("git").stdout.toString()).toContain("Usage: repo <subcommand>");
    expect(shell.exec("git is-repo").stdout.toString().trim()).toBe("true");
    expect(shell.exec("git status").stdout.toString()).toContain("git_dir:");
  });
});
