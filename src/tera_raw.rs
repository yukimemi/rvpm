//! Guards raw (unquoted) Tera `{{ ... }}` expressions used as `config.toml`
//! values (e.g. `on_event = {{ vars.on_ev_buf_open }}`) from breaking the
//! `toml_edit`-based mutate commands.
//!
//! `parse_config()` (`src/config.rs`) renders the *entire* file through Tera
//! before handing it to a TOML parser, so a bare `{{ vars.x }}` value works
//! fine for `sync` / `generate` / `update` as long as it Tera-renders into
//! valid TOML — this is the documented mechanism behind "`{{ vars.base }}`
//! and `{{ env.HOME }}` usable inside the config file" (docs/architecture.md).
//!
//! The mutate commands (`add` / `set` / `remove` / `tune`) instead parse the
//! raw file text directly with `toml_edit::DocumentMut` to preserve user
//! formatting/comments across structural edits (inserting a `[[plugins]]`
//! entry, patching one field, ...). A raw `{{ ... }}` span is not valid TOML
//! syntax on its own, so that parse used to fail outright — `rvpm add`
//! crashing with a TOML parse error on a wholly unrelated templated
//! `on_event` line elsewhere in the file, even though `rvpm sync` / `update`
//! worked fine on the same file.
//!
//! Fix: before handing raw file text to `toml_edit`, [`sanitize_tera_raw`]
//! replaces every bare `{{ ... }}` span found outside quoted strings /
//! comments with a unique quoted placeholder string (a valid TOML string
//! literal). This lets `toml_edit` parse + edit the document normally.
//! [`TeraRawGuard::restore`] then swaps each placeholder back to its
//! original raw text in the serialized output, so untouched templated lines
//! round-trip byte-for-byte and only genuinely-edited fields change.
//!
//! Scoped to `{{ ... }}` value substitution — the documented use case.
//! `{% ... %}` control-flow block tags spanning multiple lines/sections are
//! not handled; that pattern was never supported by the `toml_edit` mutate
//! path and remains out of scope here.

/// Maps sanitize-time placeholders back to their original raw Tera text.
pub(crate) struct TeraRawGuard {
    replacements: Vec<(String, String)>,
}

impl TeraRawGuard {
    /// Swap every placeholder back to its original raw `{{ ... }}` text.
    pub(crate) fn restore(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (placeholder, original) in &self.replacements {
            out = out.replace(placeholder.as_str(), original.as_str());
        }
        out
    }
}

/// Replace every bare `{{ ... }}` span outside quoted strings/comments with
/// a unique quoted placeholder so the result is safe to feed to
/// `toml_edit::DocumentMut::parse`. Pair with [`TeraRawGuard::restore`] on
/// the serialized output before writing back to disk (or before handing the
/// text onward to `parse_config`, which understands the real Tera syntax).
pub(crate) fn sanitize_tera_raw(content: &str) -> (String, TeraRawGuard) {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Normal,
        LineComment,
        BasicString,
        LiteralString,
        MultilineBasicString,
        MultilineLiteralString,
    }

    let chars: Vec<char> = content.chars().collect();
    let mut out = String::with_capacity(content.len());
    let mut replacements = Vec::new();
    let mut state = State::Normal;
    let mut i = 0usize;
    let mut counter = 0usize;

    while i < chars.len() {
        let c = chars[i];
        match state {
            State::Normal => {
                if c == '#' {
                    state = State::LineComment;
                    out.push(c);
                    i += 1;
                } else if c == '"' {
                    if chars.get(i + 1) == Some(&'"') && chars.get(i + 2) == Some(&'"') {
                        state = State::MultilineBasicString;
                        out.push_str("\"\"\"");
                        i += 3;
                    } else {
                        state = State::BasicString;
                        out.push(c);
                        i += 1;
                    }
                } else if c == '\'' {
                    if chars.get(i + 1) == Some(&'\'') && chars.get(i + 2) == Some(&'\'') {
                        state = State::MultilineLiteralString;
                        out.push_str("'''");
                        i += 3;
                    } else {
                        state = State::LiteralString;
                        out.push(c);
                        i += 1;
                    }
                } else if c == '{' && chars.get(i + 1) == Some(&'{') {
                    if let Some(end) = find_mustache_end(&chars, i + 2) {
                        let raw: String = chars[i..=end].iter().collect();
                        let placeholder = format!("__RVPM_TERA_RAW_{counter}__");
                        counter += 1;
                        out.push('"');
                        out.push_str(&placeholder);
                        out.push('"');
                        replacements.push((format!("\"{placeholder}\""), raw));
                        i = end + 1;
                    } else {
                        out.push(c);
                        i += 1;
                    }
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            State::LineComment => {
                out.push(c);
                if c == '\n' {
                    state = State::Normal;
                }
                i += 1;
            }
            State::BasicString => {
                out.push(c);
                if c == '\\'
                    && let Some(&next) = chars.get(i + 1)
                {
                    out.push(next);
                    i += 2;
                    continue;
                }
                if c == '"' {
                    state = State::Normal;
                }
                i += 1;
            }
            State::LiteralString => {
                out.push(c);
                if c == '\'' {
                    state = State::Normal;
                }
                i += 1;
            }
            State::MultilineBasicString => {
                if c == '\\' {
                    out.push(c);
                    if let Some(&next) = chars.get(i + 1) {
                        out.push(next);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    continue;
                }
                if c == '"' && chars.get(i + 1) == Some(&'"') && chars.get(i + 2) == Some(&'"') {
                    out.push_str("\"\"\"");
                    state = State::Normal;
                    i += 3;
                    continue;
                }
                out.push(c);
                i += 1;
            }
            State::MultilineLiteralString => {
                if c == '\'' && chars.get(i + 1) == Some(&'\'') && chars.get(i + 2) == Some(&'\'') {
                    out.push_str("'''");
                    state = State::Normal;
                    i += 3;
                    continue;
                }
                out.push(c);
                i += 1;
            }
        }
    }

    (out, TeraRawGuard { replacements })
}

fn find_mustache_end(chars: &[char], mut i: usize) -> Option<usize> {
    while i + 1 < chars.len() {
        if chars[i] == '}' && chars[i + 1] == '}' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml_edit::DocumentMut;

    #[test]
    fn sanitize_makes_bare_mustache_value_parseable() {
        let toml = "on_event = {{ vars.on_ev_buf_open }}\nname = \"x\"\n";
        let (sanitized, guard) = sanitize_tera_raw(toml);
        let doc: DocumentMut = sanitized.parse().expect("sanitized text must parse");
        // Round-trip: nothing touched, so restore must reproduce the original exactly.
        assert_eq!(guard.restore(&doc.to_string()), toml);
    }

    #[test]
    fn sanitize_leaves_mustache_inside_string_untouched() {
        let toml = "rev = \"{{ vars.rev }}\"\n";
        let (sanitized, guard) = sanitize_tera_raw(toml);
        assert_eq!(sanitized, toml);
        let doc: DocumentMut = sanitized.parse().unwrap();
        assert_eq!(guard.restore(&doc.to_string()), toml);
    }

    #[test]
    fn sanitize_leaves_mustache_inside_comment_untouched() {
        let toml = "# example: {{ vars.x }}\nname = \"x\"\n";
        let (sanitized, guard) = sanitize_tera_raw(toml);
        assert_eq!(sanitized, toml);
        let doc: DocumentMut = sanitized.parse().unwrap();
        assert_eq!(guard.restore(&doc.to_string()), toml);
    }

    #[test]
    fn sanitize_survives_structural_edit_of_unrelated_entry() {
        // Reproduces the reported bug: an existing plugin uses a raw
        // (unquoted) Tera value for `on_event`, and `rvpm add` needs to
        // append an unrelated new `[[plugins]]` entry without touching it.
        let toml = concat!(
            "[[plugins]]\n",
            "url = \"owner/existing\"\n",
            "on_event = {{ vars.on_ev_buf_open }}\n",
        );
        let (sanitized, guard) = sanitize_tera_raw(toml);
        let mut doc: DocumentMut = sanitized.parse().expect("sanitized text must parse");
        if doc.get("plugins").is_none() {
            doc["plugins"] = toml_edit::ArrayOfTables::new().into();
        }
        let plugins = doc["plugins"].as_array_of_tables_mut().unwrap();
        let mut new_plugin = toml_edit::Table::new();
        new_plugin["url"] = toml_edit::value("owner/new");
        plugins.push(new_plugin);

        let restored = guard.restore(&doc.to_string());
        assert!(restored.contains("on_event = {{ vars.on_ev_buf_open }}"));
        assert!(restored.contains("url = \"owner/new\""));
        assert!(restored.contains("url = \"owner/existing\""));
    }
}
