//! Turning what a command printed into text the panel can show and the
//! model can read: terminal escapes taken out, line endings made one, a
//! binary stream refused, secrets removed, and a length cap.

/// The most characters of one stream that go to the model. A large
/// `journalctl` must not spend the whole context in one card.
pub const MODEL_CHARS: usize = 16_000;

/// What one stream became.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    /// The text was cut at `limit`.
    pub truncated: bool,
}

/// Whether the bytes are not text a person could read: a NUL, or a large
/// share of control bytes.
pub fn is_binary(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    if bytes.is_empty() {
        return false;
    }
    let control = bytes
        .iter()
        .filter(|byte| {
            let byte = **byte;
            !(byte == b'\n' || byte == b'\r' || byte == b'\t' || byte >= 0x20)
        })
        .count();
    control * 100 / bytes.len() > 30
}

/// Turn one stream into text: escapes gone, CRLF and lone CR made LF,
/// secrets replaced, then cut to `limit` characters.
pub fn render(bytes: &[u8], limit: usize) -> Rendered {
    let text = String::from_utf8_lossy(bytes);
    let text = strip_escapes(&text);
    let text = normalize_newlines(&text);
    let text = redact(&text);
    truncate(&text, limit)
}

fn truncate(text: &str, limit: usize) -> Rendered {
    if text.chars().count() <= limit {
        return Rendered {
            text: text.to_string(),
            truncated: false,
        };
    }
    let mut cut: String = text.chars().take(limit).collect();
    cut.push('…');
    Rendered {
        text: cut,
        truncated: true,
    }
}

fn normalize_newlines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(character);
        }
    }
    out
}

/// Take out ANSI control sequences (colours, cursor moves, OSC titles) and
/// the other control characters a terminal protocol leaves behind.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            // Carriage returns are kept: `normalize_newlines` turns them
            // into the one line ending a card shows.
            if matches!(character, '\n' | '\r' | '\t') || !character.is_control() {
                out.push(character);
            }
            continue;
        }
        match chars.next() {
            // CSI: parameters until a final byte in 0x40..=0x7e.
            Some('[') => {
                for character in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&character) {
                        break;
                    }
                }
            }
            // OSC: until BEL or ST.
            Some(']') => {
                let mut previous = '\0';
                for character in chars.by_ref() {
                    if character == '\u{7}' || (previous == '\u{1b}' && character == '\\') {
                        break;
                    }
                    previous = character;
                }
            }
            // Any other two-character escape.
            Some(_) | None => {}
        }
    }
    out
}

/// Remove private key blocks and tokens that look like credentials.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_key = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        let newline = if line.ends_with('\n') { "\n" } else { "" };
        if in_key {
            if body.contains("-----END") {
                in_key = false;
            }
            continue;
        }
        if body.contains("-----BEGIN") && body.contains("PRIVATE KEY") {
            in_key = true;
            out.push_str("[private key removed]");
            out.push_str(newline);
            continue;
        }
        out.push_str(&redact_line(body));
        out.push_str(newline);
    }
    out
}

fn redact_line(line: &str) -> String {
    let mut after_bearer = false;
    let mut out = String::with_capacity(line.len());
    for (index, token) in line.split(' ').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let lower = token.to_ascii_lowercase();
        let is_bearer = lower.trim_matches(['"', '\'']) == "bearer"
            || lower.trim_matches(['"', '\'']) == "authorization:";
        if after_bearer || (!is_bearer && looks_like_secret(token)) {
            out.push_str("<redacted>");
        } else {
            out.push_str(token);
        }
        after_bearer = is_bearer;
    }
    out
}

fn looks_like_secret(token: &str) -> bool {
    let token = token.trim_matches(['"', '\'']);
    if token.starts_with("sk-") && token.len() > 12 {
        return true;
    }
    let lower = token.to_ascii_lowercase();
    lower.starts_with("token=")
        || lower.starts_with("key=")
        || lower.starts_with("api_key=")
        || lower.starts_with("apikey=")
        || lower.starts_with("password=")
        || lower.starts_with("secret=")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_and_cursor_moves_are_taken_out() {
        let rendered = render(b"\x1b[1;31mred\x1b[0m text\x1b]0;title\x07\n", 100);
        assert_eq!(rendered.text, "red text\n");
    }

    #[test]
    fn crlf_and_lone_cr_become_lf() {
        let rendered = render(b"a\r\nb\rc\n", 100);
        assert_eq!(rendered.text, "a\nb\nc\n");
    }

    #[test]
    fn utf8_text_survives() {
        let rendered = render("磁盘 使用率\n".as_bytes(), 100);
        assert_eq!(rendered.text, "磁盘 使用率\n");
        assert!(!rendered.truncated);
    }

    #[test]
    fn binary_output_is_recognised() {
        assert!(is_binary(b"\x00\x01\x02"));
        assert!(is_binary(&[0x01, 0x02, 0x03, 0x04]));
        assert!(!is_binary(b"plain text\n"));
        assert!(!is_binary("中文".as_bytes()));
        assert!(!is_binary(b""));
    }

    #[test]
    fn a_cut_happens_at_a_character_boundary() {
        let rendered = render("磁盘磁盘磁盘".as_bytes(), 4);
        assert_eq!(rendered.text, "磁盘磁盘…");
        assert!(rendered.truncated);
        assert_eq!(rendered.text.chars().count(), 5);
    }

    #[test]
    fn a_private_key_block_is_removed_whole() {
        let output = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAAB3Nza\n-----END OPENSSH PRIVATE KEY-----\nafter\n";
        let rendered = render(output.as_bytes(), 1000);
        assert_eq!(rendered.text, "before\n[private key removed]\nafter\n");
        assert!(!rendered.text.contains("AAAAB3Nza"));
    }

    #[test]
    fn bearer_tokens_and_keys_are_redacted() {
        let rendered = render(
            b"Authorization: Bearer abc.def.ghi\nkey=sk-1234567890abcdef\n",
            1000,
        );
        assert!(!rendered.text.contains("abc.def.ghi"));
        assert!(!rendered.text.contains("sk-1234567890abcdef"));
        assert!(rendered.text.contains("<redacted>"));
    }

    #[test]
    fn a_plain_table_keeps_its_spacing() {
        let rendered = render(
            b"Filesystem      Size  Used Avail\n/dev/sda1        50G   20G   30G\n",
            1000,
        );
        assert!(rendered.text.starts_with("Filesystem      Size"));
        assert!(!rendered.text.contains("<redacted>"));
    }
}
