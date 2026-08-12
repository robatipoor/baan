/// A streaming, state-machine parser for XML-like tags.
///
/// Feed characters one at a time with [`TagParser::consume`]. When a complete
/// `<tag>content</tag>` or `<tag/>` is parsed, [`TagParser::take`] returns the result
/// along with the total number of characters consumed since the last reset.
#[derive(Debug, Default)]
pub struct TagParser {
    state: State,
    /// Number of characters fed since the last reset to `ExpectTagName`.
    chars_consumed: usize,
    history: Vec<(State, usize)>,
}

#[derive(Debug, Default, Clone)]
enum State {
    #[default]
    ExpectTagName,
    OpeningTag {
        name: String,
    },
    SelfClosing {
        name: String,
    },
    CollectContent {
        tag_name: String,
        content: String,
    },
    ExpectCloseTag {
        tag_name: String,
        content: String,
        close_name: String,
    },
    ClosedTag {
        tag_name: String,
        content: Option<String>,
    },
}

#[allow(dead_code)]
impl TagParser {
    /// Advance the parser by one character.
    pub fn consume(&mut self, ch: char) {
        self.history.push((self.state.clone(), self.chars_consumed));
        self.chars_consumed += 1;
        self.state = match std::mem::take(&mut self.state) {
            State::ExpectTagName | State::ClosedTag { .. } => {
                if ch == '<' {
                    State::OpeningTag {
                        name: String::new(),
                    }
                } else {
                    // Not a tag character — reset the state.
                    self.clear();
                    State::ExpectTagName
                }
            }

            State::OpeningTag { mut name } => {
                if ch == '>' {
                    State::CollectContent {
                        tag_name: name,
                        content: String::new(),
                    }
                } else if ch == '/' {
                    State::SelfClosing { name }
                } else {
                    name.push(ch);
                    State::OpeningTag { name }
                }
            }

            State::SelfClosing { name } => {
                if ch == '>' {
                    State::ClosedTag {
                        tag_name: name,
                        content: None,
                    }
                } else {
                    self.clear();
                    State::ExpectTagName
                }
            }

            State::CollectContent {
                tag_name,
                mut content,
            } => {
                if ch == '<' {
                    State::ExpectCloseTag {
                        tag_name,
                        content,
                        close_name: String::new(),
                    }
                } else {
                    content.push(ch);
                    State::CollectContent { tag_name, content }
                }
            }

            State::ExpectCloseTag {
                tag_name,
                mut content,
                mut close_name,
            } => {
                if ch == '/' && close_name.is_empty() {
                    State::ExpectCloseTag {
                        tag_name,
                        content,
                        close_name,
                    }
                } else if ch == '>' {
                    if close_name == tag_name {
                        State::ClosedTag {
                            tag_name,
                            content: Some(content),
                        }
                    } else {
                        content.push_str("</");
                        content.push_str(&close_name);
                        content.push('>');
                        State::CollectContent { tag_name, content }
                    }
                } else {
                    close_name.push(ch);
                    State::ExpectCloseTag {
                        tag_name,
                        content,
                        close_name,
                    }
                }
            }
        };
    }

    /// Process a string slice character by character.
    pub fn feed(&mut self, s: &str) {
        for ch in s.chars() {
            self.consume(ch);
        }
    }

    /// If a complete tag has been parsed, returns `(tag_name, content,
    /// tag_text)` and resets the parser. `tag_text` is the full original text
    /// (e.g. `<tag>content</tag>` or `<tag/>`). Returns `None` otherwise.
    pub fn take(&mut self) -> Option<(String, Option<String>, String)> {
        if !matches!(self.state, State::ClosedTag { .. }) {
            return None;
        }
        let state = std::mem::take(&mut self.state);
        self.clear();
        if let State::ClosedTag { tag_name, content } = state {
            let tag_text = match content {
                Some(ref c) => format!("<{}>{}</{}>", tag_name, c, tag_name),
                None => format!("<{}/>", tag_name),
            };
            Some((tag_name, content, tag_text))
        } else {
            None
        }
    }

    /// Returns the number of characters consumed since the last reset.
    pub fn chars_consumed(&self) -> usize {
        self.chars_consumed
    }

    pub fn is_expecting_tag_name(&self) -> bool {
        matches!(self.state, State::ExpectTagName)
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.state, State::ClosedTag { .. })
    }

    pub fn opening_tag_name(&self) -> Option<&str> {
        if let State::OpeningTag { name } = &self.state {
            Some(name)
        } else {
            None
        }
    }

    pub fn collected_content(&self) -> Option<(&str, &str)> {
        if let State::CollectContent { tag_name, content } = &self.state {
            Some((tag_name, content))
        } else {
            None
        }
    }

    pub fn close_tag_state(&self) -> Option<(&str, &str, &str)> {
        if let State::ExpectCloseTag {
            tag_name,
            content,
            close_name,
        } = &self.state
        {
            Some((tag_name, content, close_name))
        } else {
            None
        }
    }

    /// Removes the last consumed character and reverts the parser to its previous state.
    /// Returns `true` if a character was removed, `false` if history is empty.
    pub fn remove_char(&mut self) -> bool {
        if let Some((prev_state, prev_chars)) = self.history.pop() {
            self.state = prev_state;
            self.chars_consumed = prev_chars;
            true
        } else {
            false
        }
    }

    fn clear(&mut self) {
        self.chars_consumed = 0;
        self.history.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(s: &str) -> TagParser {
        let mut p = TagParser::default();
        p.feed(s);
        p
    }

    #[test]
    fn initial_state() {
        assert!(TagParser::default().is_expecting_tag_name());
    }

    #[test]
    fn ignores_chars_before_open_bracket() {
        let mut p = TagParser::default();
        for ch in " \na".chars() {
            p.consume(ch);
            assert!(p.is_expecting_tag_name());
        }
    }

    #[test]
    fn open_bracket_starts_opening_tag() {
        let p = feed("<");
        assert_eq!(p.opening_tag_name(), Some(""));
    }

    #[test]
    fn opening_tag_builds_name() {
        let p = feed("<div");
        assert_eq!(p.opening_tag_name(), Some("div"));
    }

    #[test]
    fn closing_bracket_finishes_opening_tag() {
        let p = feed("<div>");
        assert_eq!(p.collected_content(), Some(("div", "")));
    }

    #[test]
    fn content_accumulates() {
        let p = feed("<div>hi");
        assert_eq!(p.collected_content(), Some(("div", "hi")));
    }

    #[test]
    fn open_bracket_in_content_starts_close_tag() {
        let p = feed("<div>hi<");
        assert_eq!(p.close_tag_state(), Some(("div", "hi", "")));
    }

    #[test]
    fn slash_consumed_without_storing() {
        let p = feed("<div>hi</");
        assert_eq!(p.close_tag_state(), Some(("div", "hi", "")));
    }

    #[test]
    fn close_tag_name_accumulates() {
        let p = feed("<div>hi</div");
        assert_eq!(p.close_tag_state(), Some(("div", "hi", "div")));
    }

    #[test]
    fn complete_matching_tag() {
        let p = feed("<div>hi</div>");
        assert!(p.is_closed());
    }

    #[test]
    fn take_returns_name_content_and_tag_text() {
        let mut p = TagParser::default();
        p.feed("<p>hi</p>");
        assert_eq!(
            p.take(),
            Some(("p".into(), Some("hi".into()), "<p>hi</p>".into()))
        );
        assert!(p.is_expecting_tag_name());
    }

    #[test]
    fn sequential_tags() {
        let mut p = feed("<p>a</p><b>c</b>");
        assert!(p.is_closed());
        assert_eq!(
            p.take(),
            Some(("b".into(), Some("c".into()), "<b>c</b>".into()))
        );
    }

    #[test]
    fn mismatched_close_tag_absorbed_into_content() {
        let p = feed("<div>hello</span>");
        assert_eq!(p.collected_content(), Some(("div", "hello</span>")));
    }

    #[test]
    fn tag_name_with_hyphens_and_digits() {
        let mut p = feed("<my-tag1>content</my-tag1>");
        assert_eq!(
            p.take(),
            Some((
                "my-tag1".into(),
                Some("content".into()),
                "<my-tag1>content</my-tag1>".into()
            ))
        );
    }

    #[test]
    fn chars_consumed_resets_on_non_tag_char() {
        let mut p = TagParser::default();
        p.consume('a');
        assert_eq!(p.chars_consumed(), 0);
    }

    #[test]
    fn chars_consumed_counts_full_sequence() {
        let mut p = TagParser::default();
        p.feed("<x>y</x>");
        // 8 chars total
        assert_eq!(p.chars_consumed(), 8);
    }

    #[test]
    fn single_close_tag() {
        let mut p = feed("<now/>");
        assert!(p.is_closed());
        assert_eq!(p.take(), Some(("now".into(), None, "<now/>".into())));
        assert!(p.is_expecting_tag_name());
    }

    #[test]
    fn invalid_single_close_tag_resets() {
        let p = feed("<now/a");
        assert!(p.is_expecting_tag_name());
        assert_eq!(p.chars_consumed(), 0);
    }

    #[test]
    fn test_remove_char_opening_tag() {
        let mut parser = feed("<di");
        assert_eq!(parser.chars_consumed(), 3);
        assert_eq!(parser.opening_tag_name(), Some("di"));

        // Undo 'i'
        assert!(parser.remove_char());
        assert_eq!(parser.chars_consumed(), 2);
        assert_eq!(parser.opening_tag_name(), Some("d"));

        // Undo 'd'
        assert!(parser.remove_char());
        assert_eq!(parser.chars_consumed(), 1);
        assert_eq!(parser.opening_tag_name(), Some("")); // Just '<'

        // Undo '<'
        assert!(parser.remove_char());
        assert_eq!(parser.chars_consumed(), 0);
        assert!(parser.is_expecting_tag_name());

        // History empty
        assert!(!parser.remove_char());
    }

    #[test]
    fn test_remove_char_content() {
        let mut parser = feed("<a>He");

        assert_eq!(parser.chars_consumed(), 5);
        assert_eq!(parser.collected_content(), Some(("a", "He")));

        // Backspace 'e'
        assert!(parser.remove_char());
        assert_eq!(parser.collected_content(), Some(("a", "H")));

        // Backspace 'H'
        assert!(parser.remove_char());
        assert_eq!(parser.collected_content(), Some(("a", "")));

        // Backspace '>'
        assert!(parser.remove_char());
        assert_eq!(parser.opening_tag_name(), Some("a"));
    }

    #[test]
    fn test_remove_char_invalid_input_clears_history() {
        let parser = feed("<a");
        assert_eq!(parser.chars_consumed(), 2);

        // Feed invalid char for ExpectTagName (if it resets)
        // Wait, ' ' in OpeningTag might not be invalid in standard XML,
        // but let's test a scenario that triggers `self.clear()` in your code.
        // In your code, typing 'x' at State::ExpectTagName triggers clear().

        let mut parser2 = feed("<a/>");
        // Tag is closed, chars_consumed is 4.
        assert!(parser2.is_closed());

        // Typing another char that isn't '<' triggers clear() and ExpectTagName
        parser2.consume('x');
        assert!(parser2.is_expecting_tag_name());
        assert_eq!(parser2.chars_consumed(), 0); // clear() was called

        // History should be empty now
        assert!(!parser2.remove_char());
    }

    #[test]
    fn test_remove_char_close_tag_mismatch() {
        let mut parser = feed("<a>hello</b");

        // It expects close tag 'b'
        assert_eq!(parser.close_tag_state(), Some(("a", "hello", "b")));

        // Backspace 'b'
        assert!(parser.remove_char());
        assert_eq!(parser.close_tag_state(), Some(("a", "hello", "")));

        // Backspace '/'
        assert!(parser.remove_char());
        // State goes back to ExpectCloseTag but just before typing '/'
        // Wait, looking at your code: if ch == '/' && close_name.is_empty(), it stays ExpectCloseTag.
        // But before that, '<' triggered ExpectCloseTag.
        // So undoing '/' brings us to just after '<'.
        assert_eq!(parser.close_tag_state(), Some(("a", "hello", "")));

        // Backspace '<'
        assert!(parser.remove_char());
        // Back to CollectContent
        assert_eq!(parser.collected_content(), Some(("a", "hello")));
    }
}
