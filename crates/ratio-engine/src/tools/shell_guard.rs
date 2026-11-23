use std::sync::LazyLock;

use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use tree_sitter::{Node, Parser};

const EDIT_GUIDANCE: &str = "Use create_file for new files, replace_text for unique literal replacements, or edit_file for tagged changes.";

static SCRIPT_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\b(?:write_text|write_bytes|writeFile(?:Sync)?|appendFile(?:Sync)?|createWriteStream|",
        r"writeFileAtomic|writeFileUtf8|writeFileBinary|writeAllText|writeAllBytes|appendAllText|",
        r"copyfile|copyfileSync|copy_file|movefile|set-content|add-content|out-file)\b",
        r"|\bopen\s*\([^\r\n]*?,\s*(?:mode\s*=\s*)?['\x22][wax][bt+]*['\x22]",
        r"|\b(?:open|fdopen)\s*\([^\r\n]*?,\s*(?:mode\s*=\s*)?['\x22]r[bt]*\+['\x22]",
        r"|\b(?:O_WRONLY|O_RDWR|O_CREAT|O_TRUNC|O_APPEND)\b",
        r"|\bfile_put_contents\s*\(",
    ))
    .expect("valid inline writer pattern")
});

static AWK_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)\b(?:print|printf)\b[^\n;}]*>|\bsystem\s*\(")
        .expect("valid awk writer pattern")
});

pub(super) fn check(script: &str, writable: bool) -> Result<()> {
    let result = if cfg!(windows) {
        check_cmd(script, 0)
    } else {
        check_script(script, 0)
    };
    if writable {
        result.map_err(|error| anyhow::anyhow!("{error}. {EDIT_GUIDANCE}"))
    } else {
        result
    }
}

fn check_script(script: &str, depth: usize) -> Result<()> {
    ensure!(depth < 12, "Shell command has too many nested scripts");
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_bash::LANGUAGE.into())?;
    let tree = parser
        .parse(script, None)
        .context("Could not parse shell command")?;
    ensure!(
        !tree.root_node().has_error(),
        "Could not parse shell command"
    );
    let mut pending = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "file_redirect" => check_redirect(node, script)?,
            "command" => {
                let words = command_words(node, script);
                check_command(&words, depth)?;
            }
            "heredoc_redirect" => {
                // A here-document to cat is data. For an interpreter it is code.
                if let Some(command) = node
                    .parent()
                    .and_then(|parent| parent.child_by_field_name("body").and_then(last_command))
                {
                    let words = command_words(command, script);
                    if let Some(body) = node
                        .named_children(&mut node.walk())
                        .find(|child| child.kind() == "heredoc_body")
                    {
                        let code = source(body, script);
                        if is_shell(command_name(&words)) {
                            check_script(code, depth + 1)?;
                        } else if is_interpreter(command_name(&words)) {
                            check_inline(code)?;
                        }
                    }
                }
            }
            _ => {}
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    Ok(())
}

fn last_command(body: Node<'_>) -> Option<Node<'_>> {
    let mut pending = vec![body];
    while let Some(node) = pending.pop() {
        if node.kind() == "command" {
            return Some(node);
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    None
}

fn check_redirect(node: Node<'_>, script: &str) -> Result<()> {
    let operator = node
        .children(&mut node.walk())
        .find(|child| !child.is_named())
        .map(|child| child.kind());
    if matches!(
        operator,
        Some(">" | ">>" | ">|" | "&>" | "&>>" | ">&" | "<>")
    ) {
        let target = node
            .child_by_field_name("destination")
            .map(|child| word(source(child, script)));
        let sink = target.as_deref().is_some_and(|target| {
            target == "/dev/null" || (cfg!(windows) && target.eq_ignore_ascii_case("nul"))
        });
        let descriptor = operator == Some(">&")
            && target.as_deref().is_some_and(|target| {
                target == "-"
                    || (!target.is_empty() && target.bytes().all(|byte| byte.is_ascii_digit()))
            });
        ensure!(
            sink || descriptor,
            "Shell output redirection to a file is blocked"
        );
    }
    Ok(())
}

fn source<'a>(node: Node<'_>, script: &'a str) -> &'a str {
    &script[node.byte_range()]
}

fn word(raw: &str) -> String {
    shell_words::split(raw)
        .ok()
        .filter(|words| words.len() == 1)
        .and_then(|mut words| words.pop())
        .unwrap_or_else(|| raw.to_owned())
}

fn command_words(node: Node<'_>, script: &str) -> Vec<String> {
    let mut words = Vec::new();
    if let Some(name) = node.child_by_field_name("name") {
        words.push(word(source(name, script)));
    }
    words.extend(
        node.children_by_field_name("argument", &mut node.walk())
            .map(|arg| word(source(arg, script))),
    );
    words
}

fn check_cmd(script: &str, depth: usize) -> Result<()> {
    ensure!(depth < 12, "Shell command has too many nested scripts");
    let mut chars = script.chars().peekable();
    let mut quoted = false;
    let mut token = String::new();
    let mut words = Vec::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => quoted = !quoted,
            '^' if !quoted => {
                if let Some(escaped) = chars.next() {
                    token.push(escaped);
                }
            }
            '>' if !quoted => {
                if token.len() != 1 || !token.as_bytes()[0].is_ascii_digit() {
                    finish_cmd_word(&mut token, &mut words);
                }
                token.clear();
                chars.next_if_eq(&'>');
                let descriptor = chars.next_if_eq(&'&').is_some();
                while chars.next_if(|ch| matches!(ch, ' ' | '\t')).is_some() {}
                let quoted_target = chars.next_if_eq(&'"').is_some();
                let mut target = String::new();
                if quoted_target {
                    while let Some(ch) = chars.next_if(|ch| *ch != '"') {
                        target.push(ch);
                    }
                    chars.next_if_eq(&'"');
                } else {
                    while let Some(ch) = chars.next_if(|ch| {
                        !ch.is_whitespace() && !matches!(ch, '&' | '|' | ')' | '(' | '<' | '>')
                    }) {
                        target.push(ch);
                    }
                }
                ensure!(
                    if descriptor {
                        target.len() == 1 && target.as_bytes()[0].is_ascii_digit()
                    } else {
                        target.eq_ignore_ascii_case("nul")
                    },
                    "Shell output redirection to a file is blocked"
                );
            }
            '&' | '|' | '(' | ')' | '\r' | '\n' if !quoted => {
                finish_cmd_word(&mut token, &mut words);
                check_cmd_words(&words, depth)?;
                words.clear();
            }
            ' ' | '\t' if !quoted => finish_cmd_word(&mut token, &mut words),
            _ => token.push(ch),
        }
    }
    ensure!(!quoted, "Could not parse shell command");
    finish_cmd_word(&mut token, &mut words);
    check_cmd_words(&words, depth)
}

fn finish_cmd_word(token: &mut String, words: &mut Vec<String>) {
    if !token.is_empty() {
        words.push(std::mem::take(token));
    }
}

fn check_cmd_words(mut words: &[String], depth: usize) -> Result<()> {
    while let Some((head, args)) = words.split_first() {
        match basename(head).to_ascii_lowercase().as_str() {
            "do" | "else" | "call" => words = args,
            "if" => words = cmd_if_body(args),
            _ => return check_command(words, depth),
        }
    }
    Ok(())
}

fn cmd_if_body(mut args: &[String]) -> &[String] {
    for prefix in ["/i", "not"] {
        if args
            .first()
            .is_some_and(|arg| arg.eq_ignore_ascii_case(prefix))
        {
            args = &args[1..];
        }
    }
    let condition_words = match args.first().map(|arg| arg.to_ascii_lowercase()) {
        Some(condition) => match condition.as_str() {
            "exist" | "defined" | "errorlevel" | "cmdextversion" => 2,
            condition if condition.contains("==") => 1,
            _ => 3,
        },
        None => 0,
    };
    args.get(condition_words..).unwrap_or_default()
}

fn basename(name: &str) -> &str {
    let name = name.trim_start_matches('@');
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match name.get(name.len().saturating_sub(4)..) {
        Some(suffix) if suffix.eq_ignore_ascii_case(".exe") => &name[..name.len() - 4],
        _ => name,
    }
}

fn unwrap_command(mut words: &[String]) -> &[String] {
    while let Some(name) = words.first() {
        let wrapper = basename(name);
        if !matches!(
            wrapper,
            "command" | "builtin" | "env" | "exec" | "nohup" | "time"
        ) {
            break;
        }
        if wrapper == "command"
            && words
                .get(1)
                .is_some_and(|arg| matches!(arg.as_str(), "-v" | "-V"))
        {
            return &[];
        }
        words = &words[1..];
        while words.first().is_some_and(|arg| {
            arg == "--" || arg == "-p" || (wrapper == "env" && arg.contains('='))
        }) {
            words = &words[1..];
        }
    }
    words
}

fn command_name(words: &[String]) -> &str {
    unwrap_command(words)
        .first()
        .map_or("", |name| basename(name))
}

fn is_shell(name: &str) -> bool {
    matches!(name, "sh" | "bash" | "dash" | "zsh" | "ksh")
}

fn is_interpreter(name: &str) -> bool {
    name.starts_with("python")
        || matches!(
            name,
            "node" | "nodejs" | "ruby" | "perl" | "php" | "deno" | "bun" | "powershell" | "pwsh"
        )
}

fn check_inline(code: &str) -> Result<()> {
    ensure!(
        !SCRIPT_WRITE.is_match(code),
        "Inline script contains a file-writing operation"
    );
    Ok(())
}

fn check_command(words: &[String], depth: usize) -> Result<()> {
    let Some((head, args)) = unwrap_command(words).split_first() else {
        return Ok(());
    };
    let name = basename(head).to_ascii_lowercase();
    let name = name.as_str();
    let in_place = args.iter().any(|arg| {
        arg.starts_with("--in-place")
            || (arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains('i'))
    });
    let file_writer = match name {
        "apply_patch" | "patch" | "cp" | "mv" | "truncate" | "copy" | "move" | "set-content"
        | "add-content" | "out-file" => true,
        "tee" => args.iter().any(|arg| {
            !arg.starts_with('-')
                && arg != "/dev/null"
                && !(cfg!(windows) && arg.eq_ignore_ascii_case("nul"))
        }),
        "dd" => args
            .iter()
            .any(|arg| arg.starts_with("of=") && arg != "of=/dev/null"),
        "sed" | "perl" => in_place,
        "git" => args
            .iter()
            .any(|arg| matches!(arg.as_str(), "apply" | "am" | "checkout-index")),
        "cargo" => {
            args.iter().any(|arg| arg == "fmt")
                && !args.iter().any(|arg| arg == "--check")
                && !cargo_fmt_info(args)
        }
        "rustfmt" => {
            !args.iter().any(|arg| arg == "--check" || arg == "stdout") && !rustfmt_info(args)
        }
        "gofmt" | "clang-format" | "prettier" => args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-w" | "-i" | "--write")),
        _ => false,
    };
    ensure!(
        !file_writer,
        "Shell command uses a file-writing utility or option ({name})"
    );
    if matches!(name, "eval") || head.contains(['$', '`', '%']) {
        bail!("Dynamic shell command names are blocked");
    }
    if is_shell(name)
        && let Some(code) = inline_argument(args, &["-c", "-lc", "-ec", "-xc"])
    {
        check_script(code, depth + 1)?;
    } else if name == "cmd" {
        if let Some(index) = args.iter().position(|arg| arg.eq_ignore_ascii_case("/c")) {
            check_cmd(&args[index + 1..].join(" "), depth + 1)?;
        }
    } else if is_interpreter(name) {
        if let Some(code) = inline_argument(
            args,
            &["-c", "-e", "-E", "-p", "--eval", "-Command", "-command"],
        ) {
            check_inline(code)?;
        }
    } else if matches!(name, "awk" | "gawk" | "mawk") {
        ensure!(
            !args
                .iter()
                .any(|arg| arg == "inplace" || AWK_WRITE.is_match(arg)),
            "Awk file-writing operations are blocked"
        );
    }
    Ok(())
}

fn cargo_fmt_info(args: &[String]) -> bool {
    let args = if matches!(args.first(), Some(toolchain) if toolchain.starts_with('+')) {
        &args[1..]
    } else {
        args
    };
    match args {
        [command, flag] => {
            command == "fmt" && matches!(flag.as_str(), "--version" | "--help" | "-h")
        }
        [command, separator, rest @ ..] => {
            command == "fmt" && separator == "--" && rustfmt_info(rest)
        }
        _ => false,
    }
}

fn rustfmt_info(args: &[String]) -> bool {
    matches!(args, [flag] if matches!(flag.as_str(), "--version" | "-V" | "--help" | "-h"))
}

fn inline_argument<'a>(args: &'a [String], flags: &[&str]) -> Option<&'a str> {
    args.iter().enumerate().find_map(|(index, arg)| {
        if flags.contains(&arg.as_str()) {
            args.get(index + 1).map(String::as_str)
        } else {
            let (flag, value) = arg.split_once('=')?;
            flags.contains(&flag).then_some(value)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{check, check_cmd, check_script};

    #[test]
    fn blocks_observed_write_forms_and_all_nested_redirects() {
        for script in [
            "echo 123 > README.md",
            "printf '%s' value >> 'file name'",
            "cat <<'EOF' > README.md\nhello\nEOF",
            "echo test 2>error.log",
            "echo test >&report.log",
            "cat <>new-file",
            "(echo value > nested.txt)",
            "echo \"$(printf x >> nested.txt)\"",
            "cat source | tee dest",
            "env FLAG=1 /usr/bin/sed -i '' 's/a/b/' target",
            "git apply change.diff",
            "cargo fmt --all",
            "cargo fmt --version; cargo fmt",
            "rustfmt src/lib.rs",
            "cp a b",
            "sh -c 'echo body > file'",
            "python3 -c \"open('notes.txt', 'a').write('x')\"",
            "python3 - <<'PY'\nfrom pathlib import Path\nPath('notes.txt').write_text('x')\nPY",
            "cd project && python3 <<'PY'\nopen('notes.txt', 'w').write('x')\nPY",
            "node -e \"require('fs').appendFileSync('notes.txt', 'x')\"",
        ] {
            assert!(check_script(script, 0).is_err(), "allowed write: {script}");
        }
    }

    #[test]
    fn keeps_reading_pipelines_test_commands_and_quoted_data() {
        for script in [
            "printf '%s\\n' 'x > y' | grep y",
            "git diff --check && cargo test",
            "cargo fmt --all -- --check",
            "cargo fmt --version; cargo clippy --version; rustc -vV",
            "cargo +stable fmt -- --version",
            "cargo fmt --help",
            "rustfmt -V",
            "rg needle < input.txt 2>&1 > /dev/null",
            "sed -n '1,15p' source",
            "awk 'NR > 5 { print $0 }' source",
            "cat <<'EOF'\n> merely text\nEOF",
            "python3 - <<'PY'\nfrom pathlib import Path\nprint(Path('notes.txt').read_text())\nPY",
            "yes test | head -n 200",
        ] {
            assert!(check_script(script, 0).is_ok(), "{script}");
        }
    }

    #[test]
    fn refusal_guidance_matches_whether_native_edits_are_available() {
        let command = "cargo fmt --all";
        let inspection = check(command, false).unwrap_err().to_string();
        assert!(inspection.contains("(cargo)"));
        assert!(!inspection.contains("create_file"));
        let work = check(command, true).unwrap_err().to_string();
        assert!(work.contains("create_file"));
    }

    #[test]
    fn checks_windows_redirects_and_common_command_forms() {
        for script in [
            r"for /L %i in (1,1,200) do @echo %i",
            r#"echo "a > b" | findstr b"#,
            r"echo a ^> b",
            r"git diff --check >NUL 2>&1",
        ] {
            assert!(check_cmd(script, 0).is_ok(), "{script}");
        }
        for script in [
            r"echo 123 >file.txt",
            r#"echo 123 2>>"file name""#,
            r"for /L %i in (1,1,200) do @echo %i >file.txt",
            r"if exist source @COPY source dest",
            r#"cmd.exe /D /C "echo x >file.txt""#,
            r#"python -c "open('out.txt','w').write('x')""#,
        ] {
            assert!(check_cmd(script, 0).is_err(), "{script}");
        }
    }
}
