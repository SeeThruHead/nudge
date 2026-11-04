use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Input {
    text: Vec<u8>,
    escape: Option<Vec<u8>>,
    pasting: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Key {
    Nothing,
    Changed,
    Submit(String),
    Leave,
}

const PASTE_START: &[u8] = b"[200~";
const PASTE_END: &[u8] = b"[201~";

fn finished(sequence: &[u8]) -> bool {
    match sequence {
        [] => false,
        [b'[', .., last] => (0x40..=0x7e).contains(last),
        [b'['] => false,
        [b'O'] => false,
        [b'O', _] => true,
        _ => true,
    }
}

impl Input {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.text).into_owned()
    }

    fn escape(&mut self, sequence: Vec<u8>, byte: u8) -> Key {
        let sequence: Vec<u8> = sequence.into_iter().chain([byte]).collect();
        if !finished(&sequence) {
            self.escape = Some(sequence);
            return Key::Nothing;
        }
        match sequence.as_slice() {
            PASTE_START => {
                self.pasting = true;
                Key::Nothing
            }
            PASTE_END => {
                self.pasting = false;
                Key::Changed
            }
            _ => Key::Nothing,
        }
    }

    fn erase(&mut self) -> Key {
        let start = self.text.iter().rposition(|byte| byte & 0b1100_0000 != 0b1000_0000).unwrap_or(0);
        self.text.truncate(start);
        Key::Changed
    }

    pub fn feed(&mut self, byte: u8) -> Key {
        if let Some(sequence) = self.escape.take() {
            return self.escape(sequence, byte);
        }
        match byte {
            0x1b => {
                self.escape = Some(vec![]);
                Key::Nothing
            }
            b'\r' | b'\n' if self.pasting => {
                self.text.push(b'\n');
                Key::Nothing
            }
            byte if self.pasting => {
                self.text.push(byte);
                Key::Nothing
            }
            b'\r' | b'\n' => Key::Submit(self.text().trim().to_string()),
            0x03 => Key::Leave,
            0x04 if self.text.is_empty() => Key::Leave,
            0x7f | 0x08 => self.erase(),
            0x15 => {
                self.text.clear();
                Key::Changed
            }
            b'\t' => {
                self.text.push(b' ');
                Key::Changed
            }
            byte if byte < 0x20 => Key::Nothing,
            byte => {
                self.text.push(byte);
                Key::Changed
            }
        }
    }
}

pub fn shown(text: &str, width: usize) -> String {
    let flat: Vec<char> = text.replace('\n', " ⏎ ").chars().collect();
    let room = width.saturating_sub(4).max(10);
    let skip = flat.len().saturating_sub(room);
    flat[skip..].iter().collect()
}

fn rule(width: usize, label: &str) -> String {
    let tail = if label.is_empty() { String::new() } else { format!(" {label} ─") };
    let fill = width.saturating_sub(tail.chars().count()).max(3);
    format!("{}{tail}", "─".repeat(fill))
}

fn when() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

pub fn screen(name: &str, session: &str, cwd: &str, width: usize, since: &str) -> String {
    [
        format!("\u{1b}]0;✳ {name} {}\u{7}", crate::park::PARKED_TITLE),
        "\u{1b}[?2004h\u{1b}[2J\u{1b}[H".to_string(),
        "\r\n".to_string(),
        format!("  ✳ {name} is parked. nudge closed this idle Claude session at {since} to free memory.\r\n"),
        "  Press Enter to resume it, or type a message and press Enter to resume it with that message.\r\n".into(),
        "\r\n".into(),
        format!("{}\r\n", rule(width, name)),
        "❯ \r\n".into(),
        format!("{}\r\n", rule(width, "")),
        format!("  session {session} · {cwd}\r\n"),
        "\u{1b}[3A\r❯ ".into(),
    ]
    .concat()
}

pub fn redraw(text: &str, width: usize) -> String {
    format!("\r\u{1b}[2K❯ {}", shown(text, width))
}

pub fn resume_command(shell: &str, session: &str, message: &str) -> Vec<String> {
    let base = [shell, "-ic", "claude --resume \"$@\"", "nudge-parked", session].map(String::from).to_vec();
    if message.is_empty() { base } else { base.into_iter().chain([message.to_string()]).collect() }
}

fn stty(args: &[&str]) -> Option<String> {
    Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn width() -> usize {
    stty(&["size"])
        .and_then(|size| size.split_whitespace().nth(1).and_then(|cols| cols.parse().ok()))
        .filter(|cols: &usize| *cols > 0)
        .unwrap_or(80)
}

fn shell() -> String {
    std::env::var("SHELL").ok().filter(|shell| shell.ends_with("zsh")).unwrap_or_else(|| "/bin/zsh".into())
}

fn report_session(session: &str) {
    let Ok(pane) = std::env::var("HERDR_PANE_ID") else { return };
    let herdr = std::env::var_os("HERDR_BIN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::host::find("herdr", "NUDGE_HERDR"));
    let _ = Command::new(herdr)
        .args(["pane", "report-agent-session", "--source", "herdr:claude", "--agent", "claude"])
        .args(["--agent-session-id", session, &pane])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn wait_for_input(input: &mut Input, width: usize) -> Key {
    let mut stdin = std::io::stdin().lock();
    let mut out = std::io::stdout();
    let mut byte = [0u8; 1];
    loop {
        match stdin.read(&mut byte) {
            Ok(0) | Err(_) => return Key::Leave,
            Ok(_) => {}
        }
        match input.feed(byte[0]) {
            Key::Nothing => {}
            Key::Changed => {
                let _ = out.write_all(redraw(&input.text(), width).as_bytes());
                let _ = out.flush();
            }
            done => return done,
        }
    }
}

pub fn main(args: &[String]) -> ExitCode {
    let [session, name, cwd] = args else {
        eprintln!("usage: claude <session> <name> <folder> (nudge's stand-in for a parked Claude session)");
        return ExitCode::from(2);
    };
    let _ = std::env::set_current_dir(cwd);
    report_session(session);
    let saved = stty(&["-g"]);
    let _ = stty(&["-icanon", "-echo", "-isig", "-ixon", "min", "1", "time", "0"]);
    let width = width();
    let mut out = std::io::stdout();
    let _ = out.write_all(screen(name, session, cwd, width, &when()).as_bytes());
    let _ = out.flush();
    let key = wait_for_input(&mut Input::default(), width);
    let _ = out.write_all(b"\x1b[?2004l\r\n");
    let _ = out.flush();
    let _ = saved.as_deref().map(|settings| stty(&[settings])).unwrap_or_else(|| stty(&["sane"]));
    match key {
        Key::Submit(message) => {
            let command = resume_command(&shell(), session, &message);
            let error = Command::new(&command[0]).args(&command[1..]).exec();
            eprintln!("nudge: could not start {}: {error}; resume with claude --resume {session}", command[0]);
            ExitCode::FAILURE
        }
        _ => {
            println!("Left parked. Resume it with: claude --resume {session}");
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(input: &mut Input, bytes: &[u8]) -> Vec<Key> {
        bytes.iter().map(|byte| input.feed(*byte)).filter(|key| *key != Key::Nothing).collect()
    }

    #[test]
    fn enter_alone_resumes_without_a_message() {
        assert_eq!(typed(&mut Input::default(), b"\n"), [Key::Submit(String::new())]);
        assert_eq!(typed(&mut Input::default(), b"\r"), [Key::Submit(String::new())]);
    }

    #[test]
    fn a_pasted_prompt_like_herdr_sends_comes_through_whole_with_its_line_breaks() {
        let mut input = Input::default();
        let keys = typed(&mut input, "\u{1b}[200~[nudge: ci] Check CI.\nThen fix it ✔\u{1b}[201~\n".as_bytes());

        assert_eq!(keys.last(), Some(&Key::Submit("[nudge: ci] Check CI.\nThen fix it ✔".into())));
    }

    #[test]
    fn typing_edits_the_message_and_ignores_arrow_keys() {
        let mut input = Input::default();
        let keys = typed(&mut input, "hé\u{7f}elp\u{1b}[A\u{1b}OB\tme\r".as_bytes());

        assert_eq!(keys.last(), Some(&Key::Submit("help me".into())));
        assert_eq!(typed(&mut Input::default(), b"abc\x15x\n").last(), Some(&Key::Submit("x".into())));
    }

    #[test]
    fn ctrl_c_or_ctrl_d_on_an_empty_line_leaves_it_parked() {
        assert_eq!(typed(&mut Input::default(), b"\x03"), [Key::Leave]);
        assert_eq!(typed(&mut Input::default(), b"\x04"), [Key::Leave]);
        assert_eq!(typed(&mut Input::default(), b"a\x04\n").last(), Some(&Key::Submit("a".into())));
    }

    #[test]
    fn the_screen_keeps_herdrs_claude_detection_seeing_an_idle_claude() {
        let drawn = screen("app", "abc", "/code/app", 40, "14:05");
        let lines: Vec<&str> = drawn.split("\r\n").collect();

        assert!(drawn.starts_with("\u{1b}]0;✳ app (parked)\u{7}"));
        let rule = lines.iter().find(|line| line.ends_with(" app ─")).unwrap();

        assert!(rule.starts_with('─') && rule.chars().count() == 40);
        assert!(lines.contains(&"❯ "));
        assert!(!drawn.to_lowercase().contains("esc to cancel") && !drawn.to_lowercase().contains("do you want to"));
    }

    #[test]
    fn shows_the_end_of_a_long_message_on_one_line() {
        assert_eq!(shown("one\ntwo", 80), "one ⏎ two");
        assert_eq!(shown(&"x".repeat(50).to_string(), 24), "x".repeat(20));
    }

    #[test]
    fn resumes_through_the_interactive_shell_so_the_claude_function_adds_its_flags() {
        assert_eq!(
            resume_command("/bin/zsh", "abc", ""),
            ["/bin/zsh", "-ic", "claude --resume \"$@\"", "nudge-parked", "abc"]
        );
        assert_eq!(resume_command("/bin/zsh", "abc", "hi there").last().map(String::as_str), Some("hi there"));
    }
}
