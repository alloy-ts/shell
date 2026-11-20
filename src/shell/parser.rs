//! Shell tokenizer and parser.
//!
//! # Supported syntax
//!
//! - pipelines: `a | b | c`
//! - redirections: `< in`, `> out`, `>> out`, `2> err`, `2>> err`, `&> out`,
//!   `&>> out`, `2>&1`, `1>&2`
//! - command sequences: `&&`, `||`, `;`, newline
//! - environment prefixes: `FOO=bar cmd`
//! - quoting: single quotes (fully literal), double quotes (with `\$`, `` \` ``,
//!   `\"` and `\\` escapes), backslash escapes outside quotes
//! - line continuations: a backslash immediately before a newline
//!
//! # Deliberately not supported
//!
//! The parser is **literal-only**: there is no expansion stage, and that is
//! exactly what keeps the VM free of injection surfaces.
//!
//! - variable expansion (`$VAR`, `${VAR}`)
//! - command substitution (`$(...)`, backticks)
//! - globbing (`*`, `?`, `[...]`)
//! - comments (`#` is an ordinary character)
//! - background jobs (`&`), subshells, brace expansion, job control
//!
//! # Strictness
//!
//! Parsing is strict on purpose. Anything the parser does not understand is a
//! syntax error; input is never silently dropped. A script that cannot be
//! understood completely therefore fails loudly instead of running a prefix of
//! its commands and reporting success.
//!
//! Syntax errors carry [`SYNTAX_ERROR_EXIT`] (2), matching bash.

use crate::shell::ast::{
    CommandNode, LogicalOp, PipelineNode, PipelineSequence, RedirectOp, Redirection,
};

/// Exit code reported for a syntax error, matching bash.
pub const SYNTAX_ERROR_EXIT: i32 = 2;

/// Parse a shell script into an executable [`PipelineSequence`].
pub fn parse_shell_script(input: &str) -> Result<PipelineSequence, String> {
    // Line continuations are removed at the lexical level, before word
    // splitting — exactly like a POSIX shell. Doing it here (instead of
    // inside `parse_word`) keeps a continuation from turning into an empty
    // argument and keeps `\` + newline literal inside single quotes.
    let input = strip_line_continuations(input);
    let mut parser = ShellParser::new(&input);
    parser.parse_sequence()
}

/// Remove backslash-newline pairs, honouring quote context.
///
/// Inside single quotes a backslash is an ordinary character, so the pair is
/// kept verbatim. Inside double quotes and in the unquoted word, the pair is a
/// continuation and disappears.
fn strip_line_continuations(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                out.push(c);
                i += 1;
            }
            '"' if !in_single => {
                in_double = !in_double;
                out.push(c);
                i += 1;
            }
            '\\' if !in_single => {
                match chars.get(i + 1) {
                    Some('\n') => {
                        // Continuation: drop both characters.
                        i += 2;
                    }
                    Some('\r') if chars.get(i + 2) == Some(&'\n') => {
                        // CRLF continuation.
                        i += 3;
                    }
                    Some(&next) => {
                        // An escaped character is copied verbatim; the word
                        // parser interprets the escape later.
                        out.push(c);
                        out.push(next);
                        i += 2;
                    }
                    None => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }

    out
}

/// A command that carries no executable, no arguments and no redirection.
fn is_empty_command(cmd: &CommandNode) -> bool {
    cmd.args.is_empty() && cmd.redirects.is_empty() && cmd.env_vars.is_empty()
}

struct ShellParser {
    chars: Vec<char>,
    pos: usize,
}

impl ShellParser {
    fn new(input: &str) -> Self {
        Self {
            chars: input.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_next(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.chars.get(self.pos).copied();
        if ch.is_some() {
            self.pos += 1;
        }
        ch
    }

    /// Skip spaces and tabs. Newlines are *not* skipped here: a newline
    /// separates commands and must stay visible to the sequence parser.
    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t') | Some('\r')) {
            self.pos += 1;
        }
    }

    /// Skip every whitespace character, including newlines.
    fn skip_whitespace(&mut self) {
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Match a literal string at the current position; advance on success.
    /// Allocation-free (the previous implementation built a `String` per
    /// comparison, which happened for every token of every script).
    fn match_str(&mut self, s: &str) -> bool {
        let mut i = self.pos;
        for expected in s.chars() {
            if self.chars.get(i) != Some(&expected) {
                return false;
            }
            i += 1;
        }
        self.pos = i;
        true
    }

    fn parse_sequence(&mut self) -> Result<PipelineSequence, String> {
        let mut pipelines = Vec::new();
        self.skip_whitespace();
        if self.pos >= self.chars.len() {
            return Ok(PipelineSequence { pipelines });
        }

        let first = self.parse_pipeline()?;
        pipelines.push((LogicalOp::Sequential, first));

        loop {
            // Blanks only — a newline is a separator in its own right.
            self.skip_blanks();
            if self.pos >= self.chars.len() {
                break;
            }

            let (op, op_text) = if self.match_str("&&") {
                (LogicalOp::And, "&&")
            } else if self.match_str("||") {
                (LogicalOp::Or, "||")
            } else if self.match_str(";") {
                (LogicalOp::Sequential, ";")
            } else if self.match_str("\n") {
                (LogicalOp::Sequential, "\n")
            } else {
                // Anything else is unparsed input: fail loudly rather than
                // silently ignoring the rest of the script.
                return Err(self.unexpected_token_error());
            };

            self.skip_whitespace();
            if self.pos >= self.chars.len() {
                match op {
                    // `cmd &&` with nothing after it is a syntax error, but a
                    // trailing `;` or newline is a perfectly normal ending.
                    LogicalOp::And | LogicalOp::Or => {
                        return Err(format!(
                            "Syntax error: unexpected end of input after '{op_text}'"
                        ));
                    }
                    LogicalOp::Sequential => break,
                }
            }

            let pipeline = self.parse_pipeline()?;
            pipelines.push((op, pipeline));
        }

        Ok(PipelineSequence { pipelines })
    }

    fn parse_pipeline(&mut self) -> Result<PipelineNode, String> {
        let mut commands = Vec::new();

        let first = self.parse_command()?;
        if is_empty_command(&first) {
            return Err(self.empty_command_error());
        }
        commands.push(first);

        loop {
            self.skip_blanks();
            if self.peek() == Some('|') && self.peek_next() != Some('|') {
                self.advance(); // consume '|'
                self.skip_blanks();
                let cmd = self.parse_command()?;
                if is_empty_command(&cmd) {
                    return Err(
                        "Syntax error: unexpected token '|' (empty pipeline stage)".to_string()
                    );
                }
                commands.push(cmd);
            } else {
                break;
            }
        }

        Ok(PipelineNode { commands })
    }

    fn parse_command(&mut self) -> Result<CommandNode, String> {
        let mut env_vars = Vec::new();
        let mut args = Vec::new();
        let mut redirects = Vec::new();

        let mut parsed_any_arg = false;

        loop {
            self.skip_blanks();
            let Some(ch) = self.peek() else {
                break;
            };

            // Terminating operators. `&` only terminates when it does not
            // start a `&>` / `&>>` redirection.
            if ch == '|' || ch == ';' || ch == '\n' || (ch == '&' && self.peek_next() != Some('>')) {
                break;
            }

            // Redirections
            if let Some(redir) = self.try_parse_redirection()? {
                redirects.push(redir);
                continue;
            }

            // Word / argument
            let Some(word) = self.parse_word()? else {
                break;
            };

            // Check if this is an env assignment prefix (e.g. FOO=bar) before command
            if !parsed_any_arg && is_env_assignment(&word) {
                let parts: Vec<&str> = word.splitn(2, '=').collect();
                env_vars.push((parts[0].to_string(), parts[1].to_string()));
            } else {
                parsed_any_arg = true;
                args.push(word);
            }
        }

        Ok(CommandNode {
            env_vars,
            args,
            redirects,
        })
    }

    fn try_parse_redirection(&mut self) -> Result<Option<Redirection>, String> {
        let start = self.pos;
        let mut matched_op = None;

        if self.match_str("&>>") {
            matched_op = Some(RedirectOp::BothAppend);
        } else if self.match_str("&>") {
            matched_op = Some(RedirectOp::BothTruncate);
        } else if self.match_str("2>&1") {
            return Ok(Some(Redirection {
                op: RedirectOp::StderrToStdout,
                target: String::new(),
            }));
        } else if self.match_str("1>&2") {
            return Ok(Some(Redirection {
                op: RedirectOp::StdoutToStderr,
                target: String::new(),
            }));
        } else if self.match_str("2>>") {
            matched_op = Some(RedirectOp::StderrAppend);
        } else if self.match_str("2>") {
            matched_op = Some(RedirectOp::StderrTruncate);
        } else if self.match_str("1>>") || self.match_str(">>") {
            matched_op = Some(RedirectOp::OutputAppend);
        } else if self.match_str("1>") || self.match_str(">") {
            matched_op = Some(RedirectOp::OutputTruncate);
        } else if self.match_str("<") {
            matched_op = Some(RedirectOp::Input);
        }

        if let Some(op) = matched_op {
            self.skip_blanks();
            let Some(target) = self.parse_word()? else {
                return Err("Syntax error: expected redirect target".to_string());
            };
            if target.is_empty() {
                return Err("Syntax error: empty redirect target".to_string());
            }
            return Ok(Some(Redirection { op, target }));
        }

        self.pos = start;
        Ok(None)
    }

    /// Parse a single word.
    ///
    /// Returns `None` when the current position cannot start a word (end of
    /// input or a shell metacharacter). An empty `Some("")` is a *valid* word:
    /// it comes from an explicit empty quoted string (`''`) and must be
    /// preserved as an argument.
    fn parse_word(&mut self) -> Result<Option<String>, String> {
        match self.peek() {
            None => return Ok(None),
            Some(first)
                if first.is_whitespace() || matches!(first, '|' | ';' | '&' | '<' | '>') =>
            {
                return Ok(None)
            }
            Some(_) => {}
        }

        let mut result = String::new();

        loop {
            let Some(ch) = self.peek() else {
                break;
            };
            if ch.is_whitespace() || matches!(ch, '|' | ';' | '&' | '<' | '>') {
                break;
            }

            if ch == '\'' {
                // Single quotes: preserve literal contents verbatim.
                self.advance();
                let mut closed = false;
                while let Some(c) = self.advance() {
                    if c == '\'' {
                        closed = true;
                        break;
                    }
                    result.push(c);
                }
                if !closed {
                    return Err("Syntax error: unterminated single quote".to_string());
                }
            } else if ch == '"' {
                // Double quotes: handle escape characters.
                self.advance();
                let mut closed = false;
                while let Some(c) = self.advance() {
                    if c == '"' {
                        closed = true;
                        break;
                    }
                    if c == '\\' {
                        match self.advance() {
                            // Line continuation inside double quotes.
                            Some('\n') => continue,
                            // A backslash keeps its meaning only before these.
                            Some(escaped @ ('$' | '`' | '"' | '\\')) => result.push(escaped),
                            // Otherwise the backslash is literal.
                            Some(other) => {
                                result.push('\\');
                                result.push(other);
                            }
                            None => result.push('\\'),
                        }
                    } else {
                        result.push(c);
                    }
                }
                if !closed {
                    return Err("Syntax error: unterminated double quote".to_string());
                }
            } else if ch == '\\' {
                self.advance();
                match self.advance() {
                    // Line continuation: the backslash-newline pair vanishes.
                    Some('\n') => {}
                    Some(escaped) => result.push(escaped),
                    None => {
                        return Err("Syntax error: unexpected end of input after '\\'".to_string());
                    }
                }
            } else {
                result.push(ch);
                self.advance();
            }
        }

        Ok(Some(result))
    }

    /// Error for input the sequence parser cannot interpret.
    fn unexpected_token_error(&self) -> String {
        let Some(ch) = self.peek() else {
            return "Syntax error: unexpected end of input".to_string();
        };
        if ch == '&' {
            return "Syntax error: background operator '&' is not supported".to_string();
        }
        format!("Syntax error: unexpected token '{ch}' at position {}", self.pos)
    }

    /// Error for a command that turned out to have no content.
    fn empty_command_error(&self) -> String {
        match self.peek() {
            Some(';') => "Syntax error: unexpected token ';'".to_string(),
            Some('|') => "Syntax error: unexpected token '|'".to_string(),
            Some('&') => "Syntax error: unexpected token '&'".to_string(),
            Some('\n') => "Syntax error: unexpected token newline".to_string(),
            Some(ch) => format!("Syntax error: unexpected token '{ch}'"),
            None => "Syntax error: unexpected end of input".to_string(),
        }
    }
}

fn is_env_assignment(s: &str) -> bool {
    if let Some(idx) = s.find('=') {
        if idx == 0 {
            return false;
        }
        let var_name = &s[..idx];
        var_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !var_name.chars().next().unwrap().is_ascii_digit()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_command() {
        let seq = parse_shell_script("echo hello world").unwrap();
        assert_eq!(seq.pipelines.len(), 1);
        let cmd = &seq.pipelines[0].1.commands[0];
        assert_eq!(cmd.args, vec!["echo", "hello", "world"]);
    }

    #[test]
    fn parse_pipeline() {
        let seq = parse_shell_script("cat file.txt | grep foo | wc -l").unwrap();
        assert_eq!(seq.pipelines.len(), 1);
        let p = &seq.pipelines[0].1;
        assert_eq!(p.commands.len(), 3);
        assert_eq!(p.commands[0].args, vec!["cat", "file.txt"]);
        assert_eq!(p.commands[1].args, vec!["grep", "foo"]);
        assert_eq!(p.commands[2].args, vec!["wc", "-l"]);
    }

    #[test]
    fn parse_redirection() {
        let seq = parse_shell_script("echo 'hello' > out.txt 2>&1").unwrap();
        let cmd = &seq.pipelines[0].1.commands[0];
        assert_eq!(cmd.args, vec!["echo", "hello"]);
        assert_eq!(cmd.redirects.len(), 2);
        assert_eq!(cmd.redirects[0].op, RedirectOp::OutputTruncate);
        assert_eq!(cmd.redirects[0].target, "out.txt");
        assert_eq!(cmd.redirects[1].op, RedirectOp::StderrToStdout);
    }

    // --- `&>` / `&>>` used to be unreachable: `&` terminated the command
    // before the redirection parser ran, so both operators were dropped. ---

    #[test]
    fn parse_both_redirect_operators() {
        let seq = parse_shell_script("echo hi &> out.txt").unwrap();
        let cmd = &seq.pipelines[0].1.commands[0];
        assert_eq!(cmd.args, vec!["echo", "hi"]);
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::BothTruncate);
        assert_eq!(cmd.redirects[0].target, "out.txt");

        let seq = parse_shell_script("echo hi &>> out.txt").unwrap();
        let cmd = &seq.pipelines[0].1.commands[0];
        assert_eq!(cmd.redirects[0].op, RedirectOp::BothAppend);
        assert_eq!(cmd.redirects[0].target, "out.txt");
    }

    #[test]
    fn background_operator_is_rejected() {
        let err = parse_shell_script("echo one & echo two").unwrap_err();
        assert!(err.contains('&'), "unexpected error: {err}");
    }

    // --- Unparsed trailing input must be an error, not silence. ---

    #[test]
    fn trailing_input_is_rejected() {
        let err = parse_shell_script("echo one & echo two").unwrap_err();
        assert!(err.contains("Syntax error"), "unexpected error: {err}");
        let err = parse_shell_script("echo one garbage &&").unwrap_err();
        assert!(err.contains("Syntax error"), "unexpected error: {err}");
    }

    #[test]
    fn trailing_separator_is_allowed() {
        assert!(parse_shell_script("echo hi;").is_ok());
        assert!(parse_shell_script("echo hi\n").is_ok());
        assert!(parse_shell_script("echo hi\n\n").is_ok());
        assert!(parse_shell_script("").is_ok());
        assert!(parse_shell_script("   \n ").is_ok());
    }

    #[test]
    fn dangling_boolean_operator_is_rejected() {
        let err = parse_shell_script("echo hi &&").unwrap_err();
        assert!(err.contains("end of input"), "unexpected error: {err}");
        let err = parse_shell_script("echo hi ||").unwrap_err();
        assert!(err.contains("end of input"), "unexpected error: {err}");
    }

    // --- Quotes ---

    #[test]
    fn unterminated_quotes_are_rejected() {
        let err = parse_shell_script("echo 'unterminated").unwrap_err();
        assert!(err.contains("single quote"), "unexpected error: {err}");
        let err = parse_shell_script("echo \"unterminated").unwrap_err();
        assert!(err.contains("double quote"), "unexpected error: {err}");
    }

    #[test]
    fn empty_quoted_argument_is_preserved() {
        let seq = parse_shell_script("echo '' x").unwrap();
        let cmd = &seq.pipelines[0].1.commands[0];
        assert_eq!(cmd.args, vec!["echo", "", "x"]);
    }

    #[test]
    fn double_quote_backslash_rules() {
        // `\` keeps its meaning before $ ` " \ ...
        let seq = parse_shell_script(r#"echo "a\$b \"c\" \\d""#).unwrap();
        let cmd = &seq.pipelines[0].1.commands[0];
        assert_eq!(cmd.args[1], r#"a$b "c" \d"#);

        // ... and is literal everywhere else.
        let seq = parse_shell_script(r#"echo "a\qb""#).unwrap();
        let cmd = &seq.pipelines[0].1.commands[0];
        assert_eq!(cmd.args[1], r"a\qb");
    }

    #[test]
    fn single_quotes_are_fully_literal() {
        let seq = parse_shell_script(r"echo 'a\b$c'").unwrap();
        let cmd = &seq.pipelines[0].1.commands[0];
        assert_eq!(cmd.args[1], r"a\b$c");
    }

    // --- Line continuations ---

    #[test]
    fn backslash_newline_is_a_continuation() {
        let seq = parse_shell_script("echo a \\\n  b").unwrap();
        let cmd = &seq.pipelines[0].1.commands[0];
        assert_eq!(cmd.args, vec!["echo", "a", "b"]);
    }

    #[test]
    fn newline_separates_commands() {
        let seq = parse_shell_script("echo a\necho b").unwrap();
        assert_eq!(seq.pipelines.len(), 2);
        assert_eq!(seq.pipelines[0].1.commands[0].args, vec!["echo", "a"]);
        assert_eq!(seq.pipelines[1].1.commands[0].args, vec!["echo", "b"]);
        assert_eq!(seq.pipelines[1].0, LogicalOp::Sequential);
    }

    #[test]
    fn empty_pipeline_stage_is_rejected() {
        let err = parse_shell_script("echo a | | echo b").unwrap_err();
        assert!(err.contains("Syntax error"), "unexpected error: {err}");
        let err = parse_shell_script("; echo b").unwrap_err();
        assert!(err.contains("Syntax error"), "unexpected error: {err}");
    }

    #[test]
    fn redirection_only_command_is_allowed() {
        let seq = parse_shell_script("> out.txt").unwrap();
        let cmd = &seq.pipelines[0].1.commands[0];
        assert!(cmd.args.is_empty());
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::OutputTruncate);
    }

    #[test]
    fn missing_redirect_target_is_rejected() {
        let err = parse_shell_script("echo hi >").unwrap_err();
        assert!(err.contains("redirect target"), "unexpected error: {err}");
    }

    #[test]
    fn env_assignment_prefix() {
        let seq = parse_shell_script("FOO=bar BAZ=qux echo hi").unwrap();
        let cmd = &seq.pipelines[0].1.commands[0];
        assert_eq!(
            cmd.env_vars,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string())
            ]
        );
        assert_eq!(cmd.args, vec!["echo", "hi"]);
    }
}
