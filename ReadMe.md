# shell

A sandboxed shell runtime: a Rust core (NAPI-RS) with a thin TypeScript
wrapper. It parses a shell script and executes it inside a VM where **every
external executable must be explicitly allowed**.

## Getting Started

1. Clone the repository
2. Install dependencies
3. Run the application

```bash
git clone https://repo.chen.software/yumin-chen/shell.git
cd shell
npm install
npm run start
```

## Running the application

```bash
npm run start
```

## Testing the application

```bash
npm run test
```

## Shell syntax

Supported:

| Feature | Examples |
| --- | --- |
| Pipelines | `a \| b \| c` |
| Redirections | `< in`, `> out`, `>> out`, `2> err`, `2>> err`, `&> out`, `&>> out`, `2>&1`, `1>&2` |
| Command sequences | `&&`, `\|\|`, `;`, newline |
| Environment prefixes | `FOO=bar cmd` |
| Comments | `# comment` at the start of a word |
| Quoting | `'literal'`, `"escapes \$ \" \\ \`"`, `\x` |
| Line continuations | a backslash immediately before a newline |

Deliberately **not** supported — the parser is literal-only, and the absence of
an expansion stage is what keeps the VM free of injection surfaces:

- variable expansion (`$VAR`, `${VAR}`)
- command substitution (`$(...)`, backticks)
- globbing (`*`, `?`, `[...]`)
- background jobs (`&`), subshells, brace expansion, job control

This is a POSIX-style **literal subset**, not a full POSIX shell or Bash. The
parser rejects unsupported expansion and control-flow syntax instead of
running it with a different meaning. `&>` and `&>>` are optional extensions,
not POSIX operators. Commands and file redirections still use the sandboxed
VM; file descriptors, process concurrency and expansion semantics differ from
`sh`. Do not use this runtime to validate or execute arbitrary POSIX scripts.

### Strict parsing

Parsing is strict on purpose: **input is never silently dropped**. Anything the
parser does not understand is a syntax error, so a script can never run a
prefix of its commands and report success.

```bash
shell.run("echo one & echo two"); // exit 2 — background operator is unsupported
shell.run("echo 'unterminated");   // exit 2 — unterminated single quote
shell.run("echo one\necho two");   // both lines run, stdout is "one\ntwo\n"
```

## Allow-surfaces

All four are deny-by-default. The TypeScript wrapper additionally defaults
`allowHostExecutables` to `node` and `npm` (absolute paths) when the caller
does not pass one explicitly; every other surface starts empty.

| Option | Shape | Resolution |
| --- | --- | --- |
| `allowHostExecutables` | `{ name: absolutePath }` | Host executables. Commands not listed are denied with exit 126. |
| `allowLocalExecutables` | `{ name: relativePath }` | Project-relative paths, resolved against the shell cwd at run time. A missing target reports exit 127. |
| `allowPackageScripts` | `string[]` (`"*"` = all) | `package.json` scripts, executed **inside the VM** with the same allow-surfaces — a script cannot invoke anything you have not allowed. |
| `allowNodeModulesBin` | `string[]` (`"*"` = all) | `node_modules/.bin` entries, resolved by walking up from the cwd and spawned directly (full host access — allow per name, not with `"*"`). |

```ts
const shell = new Shell({
  cwd: process.cwd(),
  allowHostExecutables: { git: "/usr/bin/git" },
  allowPackageScripts: ["build"],
  allowNodeModulesBin: ["vitest"],
  allowLocalExecutables: { dev: "scripts/dev.sh" },
});

const result = shell.run("vitest --run");
```

Filesystem builtins (`cat`, `ls`, `rm`, `mv`, `touch`, `mkdir`, `grep`, `wc`,
`awk`, `cd`) and every redirection target are confined to the shell cwd:
absolute paths, `..` traversal and symlinks pointing outside are rejected.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | The command failed |
| `2` | Syntax error (matching bash) |
| `126` | The command is not permitted by any allow-surface |
| `127` | The command was allowed but could not be found |
