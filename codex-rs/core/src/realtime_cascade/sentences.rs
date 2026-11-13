//! Streams assistant Markdown into speakable sentences.
//!
//! Rules (tested one by one in `sentences_tests.rs`):
//! - a sentence ends after `。！？；…`, after `.!?;` followed by whitespace, or at a line break;
//! - a sentence longer than [`MAX_SENTENCE_CHARS`] is cut at its last comma or space before the
//!   limit (or at the limit);
//! - fenced code blocks are skipped whole; list, heading and quote markers at the start of a line
//!   are dropped;
//! - [`speakable`] removes inline Markdown (emphasis, code ticks, links, URLs, table pipes) and
//!   keeps a sentence only if it has a letter or digit.

/// Longest sentence sent to the speech endpoint in one request.
pub(crate) const MAX_SENTENCE_CHARS: usize = 240;

const CJK_TERMINATORS: [char; 6] = ['。', '！', '？', '；', '…', '\u{FF0E}'];
const ASCII_TERMINATORS: [char; 4] = ['.', '!', '?', ';'];
const SOFT_BREAKS: [char; 5] = [',', '，', '、', ' ', '：'];

#[derive(Debug)]
pub(crate) struct SentenceSplitter {
    buffer: String,
    in_code_block: bool,
    at_line_start: bool,
}

impl Default for SentenceSplitter {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            in_code_block: false,
            at_line_start: true,
        }
    }
}

/// What a line's start says once enough of it is known.
enum LineStart {
    NeedMore,
    Fence,
    Marker(usize),
    Text,
}

fn classify_line_start(text: &str, complete: bool) -> LineStart {
    let trimmed = text.trim_start_matches([' ', '\t']);
    let indent = text.len() - trimmed.len();
    let short = |needed: usize| !complete && trimmed.len() < needed && !trimmed.contains('\n');
    if short(3) {
        return LineStart::NeedMore;
    }
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        return LineStart::Fence;
    }
    let bytes = trimmed.as_bytes();
    // `#`… headings and `>` quotes.
    let hashes = bytes
        .iter()
        .take_while(|&&b| b == b'#' || b == b'>')
        .count();
    if hashes > 0 {
        if hashes == bytes.len() && !complete {
            return LineStart::NeedMore;
        }
        if bytes.get(hashes).is_some_and(|b| *b == b' ') {
            return LineStart::Marker(indent + hashes + 1);
        }
        return LineStart::Text;
    }
    // `-`, `*`, `+` bullets.
    if matches!(bytes.first(), Some(b'-' | b'*' | b'+')) && bytes.get(1) == Some(&b' ') {
        return LineStart::Marker(indent + 2);
    }
    // `12.` / `3)` ordered list markers.
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && digits <= 3 {
        match (bytes.get(digits), bytes.get(digits + 1)) {
            (Some(b'.' | b')'), Some(b' ')) => return LineStart::Marker(indent + digits + 2),
            (None, _) | (Some(b'.' | b')'), None) if !complete => return LineStart::NeedMore,
            _ => {}
        }
    }
    LineStart::Text
}

/// Byte index just past the first sentence end in `text`, the end of what was consumed and
/// whether a line break ended it; `None` when no end is known yet.
fn find_sentence_end(text: &str, complete: bool) -> Option<(usize, usize, bool)> {
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if c == '\n' {
            return Some((index, index + 1, true));
        }
        let end = index + c.len_utf8();
        if CJK_TERMINATORS.contains(&c) {
            return Some((end, end, false));
        }
        if ASCII_TERMINATORS.contains(&c) {
            match chars.peek() {
                Some((_, next)) if next.is_whitespace() => return Some((end, end, false)),
                Some(_) => {}
                None if complete => return Some((end, end, false)),
                None => return None,
            }
        }
    }
    None
}

/// Byte index to cut an over-long sentence at: after the last soft break within the limit.
fn cut_point(text: &str) -> usize {
    let limit = text
        .char_indices()
        .nth(MAX_SENTENCE_CHARS)
        .map_or(text.len(), |(index, _)| index);
    text[..limit]
        .char_indices()
        .rev()
        .find(|(index, c)| SOFT_BREAKS.contains(c) && *index > 0)
        .map_or(limit, |(index, c)| index + c.len_utf8())
}

impl SentenceSplitter {
    /// Adds streamed text; returns the sentences it completed.
    pub(crate) fn push(&mut self, delta: &str) -> Vec<String> {
        self.buffer.push_str(delta);
        self.drain(/*complete*/ false)
    }

    /// Ends the stream; returns the sentences left in the buffer.
    pub(crate) fn finish(&mut self) -> Vec<String> {
        let sentences = self.drain(/*complete*/ true);
        *self = Self::default();
        sentences
    }

    fn drain(&mut self, complete: bool) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            if self.buffer.is_empty() {
                break;
            }
            if self.in_code_block {
                if !self.skip_code_line() {
                    break;
                }
                continue;
            }
            if self.at_line_start {
                match classify_line_start(&self.buffer, complete) {
                    LineStart::NeedMore => break,
                    LineStart::Fence => {
                        // The opening fence line (with its language tag) is dropped whole.
                        match self.buffer.find('\n') {
                            Some(newline) => {
                                self.buffer.drain(..=newline);
                                self.in_code_block = true;
                                continue;
                            }
                            None if complete => {
                                self.buffer.clear();
                                break;
                            }
                            None => break,
                        }
                    }
                    LineStart::Marker(len) => {
                        self.buffer.drain(..len);
                        self.at_line_start = false;
                        continue;
                    }
                    LineStart::Text => self.at_line_start = false,
                }
            }
            if let Some((end, consumed, line_break)) = find_sentence_end(&self.buffer, complete) {
                if let Some(sentence) = speakable(&self.buffer[..end]) {
                    out.push(sentence);
                }
                self.buffer.drain(..consumed);
                self.at_line_start = line_break;
                continue;
            }
            if self.buffer.chars().count() > MAX_SENTENCE_CHARS {
                let cut = cut_point(&self.buffer);
                if let Some(sentence) = speakable(&self.buffer[..cut]) {
                    out.push(sentence);
                }
                self.buffer.drain(..cut);
                continue;
            }
            if complete {
                if let Some(sentence) = speakable(&self.buffer) {
                    out.push(sentence);
                }
                self.buffer.clear();
            }
            break;
        }
        out
    }

    /// Inside a code block: drops one line, leaving the block at a closing fence. Returns false
    /// when the line is not complete yet (everything known is dropped meanwhile).
    fn skip_code_line(&mut self) -> bool {
        match self.buffer.find('\n') {
            Some(newline) => {
                let line = self.buffer[..newline].trim_start_matches([' ', '\t']);
                let closes = self.at_line_start
                    && !line.is_empty()
                    && (line.starts_with("```") || line.starts_with("~~~"))
                    && line.trim_end().chars().all(|c| c == '`' || c == '~');
                if closes {
                    self.in_code_block = false;
                }
                self.buffer.drain(..=newline);
                self.at_line_start = true;
                true
            }
            None => {
                // Keep a possible closing fence's start; drop anything else.
                let tail = self.buffer.trim_start_matches([' ', '\t']);
                let maybe_fence = self.at_line_start
                    && tail.len() < 4
                    && tail.chars().all(|c| c == '`' || c == '~');
                if !maybe_fence {
                    self.buffer.clear();
                    self.at_line_start = false;
                }
                false
            }
        }
    }
}

/// Removes inline Markdown and URLs; `None` when nothing worth speaking is left.
pub(crate) fn speakable(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        // `![alt](url)` and `[text](url)` keep only their text.
        if c == '!' || c == '[' {
            let open = usize::from(c == '!');
            if rest[open..].starts_with('[')
                && let Some(close) = rest[open..].find("](")
                && let Some(end) = rest[open + close..].find(')')
            {
                out.push_str(&rest[open + 1..open + close]);
                rest = &rest[open + close + end + 1..];
                continue;
            }
        }
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            rest = &rest[end..];
            continue;
        }
        match c {
            '*' | '`' | '|' | '#' | '~' => out.push(' '),
            _ => out.push(c),
        }
        rest = &rest[c.len_utf8()..];
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .chars()
        .any(char::is_alphanumeric)
        .then_some(collapsed)
}

#[cfg(test)]
#[path = "sentences_tests.rs"]
mod tests;
