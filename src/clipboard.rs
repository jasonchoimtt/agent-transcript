use std::io::Write;

use base64::Engine as _;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, PartialEq)]
pub enum ClipboardBackend {
    Pbcopy,
    WlCopy,
    Xclip,
    Xsel,
    Osc52,
}

impl ClipboardBackend {
    pub fn display_name(&self) -> &str {
        match self {
            ClipboardBackend::Pbcopy => "pbcopy",
            ClipboardBackend::WlCopy => "wl-copy",
            ClipboardBackend::Xclip => "xclip",
            ClipboardBackend::Xsel => "xsel",
            ClipboardBackend::Osc52 => "OSC 52",
        }
    }
}

fn command_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn is_macos() -> bool {
    std::env::var("OSTYPE")
        .map(|v| v.starts_with("darwin"))
        .unwrap_or(false)
}

pub fn detect() -> ClipboardBackend {
    if is_macos() {
        return ClipboardBackend::Pbcopy;
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && command_exists("wl-copy") {
        return ClipboardBackend::WlCopy;
    }
    if std::env::var_os("DISPLAY").is_some() {
        if command_exists("xclip") {
            return ClipboardBackend::Xclip;
        }
        if command_exists("xsel") {
            return ClipboardBackend::Xsel;
        }
    }
    ClipboardBackend::Osc52
}

pub fn copy(backend: &ClipboardBackend, text: &str) -> Result<(), String> {
    match backend {
        ClipboardBackend::Pbcopy => run_cmd("pbcopy", &[], text),
        ClipboardBackend::WlCopy => run_cmd("wl-copy", &[], text),
        ClipboardBackend::Xclip => run_cmd("xclip", &["-selection", "clipboard"], text),
        ClipboardBackend::Xsel => run_cmd("xsel", &["--clipboard", "--input"], text),
        ClipboardBackend::Osc52 => copy_osc52(text),
    }
}

fn run_cmd(cmd: &str, args: &[&str], input: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(input.as_bytes())
            .map_err(|e| format!("{cmd}: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("{cmd}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} exited with {status}"))
    }
}

fn copy_osc52(text: &str) -> Result<(), String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{encoded}\x07");
    std::io::stdout()
        .write_all(seq.as_bytes())
        .and_then(|()| std::io::stdout().flush())
        .map_err(|e| format!("OSC 52: {e}"))
}

pub fn markdown_to_plain(md: &str) -> String {
    let mut output = String::new();
    let parser = Parser::new_ext(md, Options::all());
    for event in parser {
        match event {
            Event::Text(t) | Event::Code(t) => {
                output.push_str(&t);
            }
            Event::SoftBreak | Event::HardBreak => {
                output.push('\n');
            }
            Event::End(TagEnd::Paragraph)
            | Event::End(TagEnd::Heading(_))
            | Event::End(TagEnd::CodeBlock) => {
                if !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push('\n');
            }
            Event::Start(Tag::Item) => {
                if !output.is_empty() && !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str("- ");
            }
            _ => {}
        }
    }
    output.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passthrough() {
        assert_eq!(markdown_to_plain("hello world"), "hello world");
    }

    #[test]
    fn strips_bold_italic() {
        assert_eq!(
            markdown_to_plain("**bold** and *italic*"),
            "bold and italic"
        );
    }

    #[test]
    fn strips_heading() {
        let plain = markdown_to_plain("# Heading\n\nParagraph.");
        assert!(plain.contains("Heading"));
        assert!(plain.contains("Paragraph."));
        assert!(!plain.contains('#'));
    }

    #[test]
    fn inline_code_preserved() {
        assert_eq!(markdown_to_plain("use `foo()` here"), "use foo() here");
    }

    #[test]
    fn fenced_code_block() {
        let md = "```rust\nfn main() {}\n```\n\nafter";
        let plain = markdown_to_plain(md);
        assert!(plain.contains("fn main() {}"));
        assert!(plain.contains("after"));
        assert!(!plain.contains("```"));
    }

    #[test]
    fn list_items_have_prefix() {
        let md = "- item one\n- item two";
        let plain = markdown_to_plain(md);
        assert!(plain.contains("item one"));
        assert!(plain.contains("item two"));
    }
}
