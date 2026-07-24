//! `key:value` tags in a search string.
//!
//! The CLI has always been able to scope a search with `--author` and friends,
//! but the full-screen interface has only one text box. Rather than grow a
//! second UI for it, the box understands tags:
//!
//! ```text
//! author:herbert dune
//! title:"the dispossessed" lang:english
//! le guin ext:epub year:1974
//! ```
//!
//! The same syntax works in a bare CLI query, so `tomesole author:tolkien` and
//! `tomesole --author tolkien` do the same thing.
//!
//! A word that is not a recognised tag is left alone, so a title containing a
//! colon — `Dune: Messiah`, `http://…` — searches for exactly what was typed.

use crate::model::{Field, SearchQuery, Topic};

/// What a search string decomposed into.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tagged {
    /// Everything that was not a filter tag, in the order it was typed.
    pub terms: String,
    /// Columns to restrict the search to.
    pub fields: Vec<Field>,
    pub topics: Vec<Topic>,
    pub extension: Option<String>,
    pub language: Option<String>,
}

impl Tagged {
    /// True when the string carried at least one tag.
    pub fn is_tagged(&self) -> bool {
        !self.fields.is_empty()
            || !self.topics.is_empty()
            || self.extension.is_some()
            || self.language.is_some()
    }
}

/// The tags this understands, and what they mean. Also the help text: the
/// interface lists them straight from here so the two cannot drift apart.
pub const TAGS: &[(&str, &str)] = &[
    ("author:", "match the author column"),
    ("title:", "match the title column"),
    ("series:", "match the series column"),
    ("publisher:", "match the publisher column"),
    ("year:", "match the year column"),
    ("isbn:", "look up an ISBN"),
    ("ext:", "keep only this format, e.g. epub"),
    ("lang:", "keep only this language"),
    ("topic:", "search a collection, e.g. fiction"),
];

/// What a tag key does.
enum Tag {
    Column(Field),
    Extension,
    Language,
    Topic,
}

fn tag_for(key: &str) -> Option<Tag> {
    Some(match key.to_ascii_lowercase().as_str() {
        "author" | "a" | "by" => Tag::Column(Field::Author),
        "title" | "t" => Tag::Column(Field::Title),
        "series" | "s" => Tag::Column(Field::Series),
        "publisher" | "pub" | "p" => Tag::Column(Field::Publisher),
        "year" | "y" => Tag::Column(Field::Year),
        "isbn" => Tag::Column(Field::Isbn),
        "ext" | "extension" | "format" | "fmt" => Tag::Extension,
        "lang" | "language" | "l" => Tag::Language,
        "topic" | "collection" => Tag::Topic,
        _ => return None,
    })
}

/// Split a search string into free text and tags.
pub fn parse(input: &str) -> Tagged {
    let mut out = Tagged::default();
    let mut terms: Vec<String> = Vec::new();

    for token in tokenize(input) {
        let Some(key) = token.tag.clone() else {
            if !token.value.is_empty() {
                terms.push(token.value);
            }
            continue;
        };
        // An unrecognised key is not a tag at all: put the colon back and
        // search for whatever was typed. `Dune: Messiah` must survive.
        let Some(tag) = tag_for(&key) else {
            terms.push(token.raw);
            continue;
        };
        // A tag with nothing after it is a half-typed line, not an instruction.
        let value = token.value.as_str();
        if value.trim().is_empty() {
            continue;
        }
        match tag {
            Tag::Column(field) => {
                if !out.fields.contains(&field) {
                    out.fields.push(field);
                }
                // The mirror matches one request string against the columns it
                // was given, so a column tag still contributes its value to the
                // query rather than filtering afterwards.
                terms.push(value.to_string());
            }
            Tag::Extension => {
                out.extension = Some(value.trim_start_matches('.').to_ascii_lowercase())
            }
            Tag::Language => out.language = Some(value.to_string()),
            Tag::Topic => {
                for name in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if let Some(topic) = Topic::parse(name)
                        && !out.topics.contains(&topic)
                    {
                        out.topics.push(topic);
                    }
                }
            }
        }
    }

    out.terms = terms.join(" ");
    out
}

/// Fold a search string's tags into a query, leaving anything already set by
/// an explicit flag alone — flags are the more deliberate statement.
pub fn apply(input: &str, query: &mut SearchQuery) {
    let tagged = parse(input);
    query.terms = tagged.terms;
    for field in tagged.fields {
        if !query.fields.contains(&field) {
            query.fields.push(field);
        }
    }
    if !tagged.topics.is_empty() {
        query.topics = tagged.topics;
    }
    if query.extension.is_none() {
        query.extension = tagged.extension;
    }
    if query.language.is_none() {
        query.language = tagged.language;
    }
}

/// One whitespace-separated chunk of the input.
struct Token {
    /// The key, when the chunk looked like `key:value`.
    tag: Option<String>,
    /// The value, unquoted.
    value: String,
    /// The chunk as typed, for putting back when the key means nothing.
    raw: String,
}

/// Split on whitespace, keeping quoted runs together.
fn tokenize(input: &str) -> Vec<Token> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }

        // A key is letters followed by a colon. Anything else starts a value.
        let start = i;
        let mut key = String::new();
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            key.push(chars[i]);
            i += 1;
        }
        let tag = if !key.is_empty() && i < chars.len() && chars[i] == ':' {
            i += 1;
            Some(key)
        } else {
            i = start;
            None
        };

        let (value, end) = read_value(&chars, i);
        i = end;
        tokens.push(Token {
            tag,
            value,
            raw: chars[start..i].iter().collect(),
        });
    }
    tokens
}

/// Read one value: a quoted run, or everything up to the next space.
fn read_value(chars: &[char], mut i: usize) -> (String, usize) {
    let mut value = String::new();
    if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
        let quote = chars[i];
        i += 1;
        while i < chars.len() && chars[i] != quote {
            value.push(chars[i]);
            i += 1;
        }
        // Step over the closing quote when there is one; an unterminated quote
        // simply runs to the end of the line, which is what someone still
        // typing would expect.
        if i < chars.len() {
            i += 1;
        }
        return (value, i);
    }
    while i < chars.len() && !chars[i].is_whitespace() {
        value.push(chars[i]);
        i += 1;
    }
    (value, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_left_alone() {
        let t = parse("the pragmatic programmer");
        assert_eq!(t.terms, "the pragmatic programmer");
        assert!(!t.is_tagged());
    }

    #[test]
    fn a_column_tag_scopes_the_search_and_keeps_its_value() {
        let t = parse("author:herbert");
        assert_eq!(t.terms, "herbert");
        assert_eq!(t.fields, [Field::Author]);
        assert!(t.is_tagged());
    }

    #[test]
    fn quoted_values_hold_together() {
        let t = parse("title:\"the dispossessed\" author:'le guin'");
        assert_eq!(t.terms, "the dispossessed le guin");
        assert_eq!(t.fields, [Field::Title, Field::Author]);
    }

    #[test]
    fn filters_do_not_leak_into_the_query_terms() {
        let t = parse("dune ext:.EPUB lang:English");
        assert_eq!(t.terms, "dune", "filter values are not search words");
        assert_eq!(t.extension.as_deref(), Some("epub"));
        assert_eq!(t.language.as_deref(), Some("English"));
    }

    #[test]
    fn tags_mix_with_free_text_in_order() {
        let t = parse("le guin title:dispossessed 1974");
        assert_eq!(t.terms, "le guin dispossessed 1974");
        assert_eq!(t.fields, [Field::Title]);
    }

    #[test]
    fn short_aliases_work() {
        assert_eq!(parse("a:tolkien").fields, [Field::Author]);
        assert_eq!(parse("t:hobbit").fields, [Field::Title]);
        assert_eq!(parse("by:tolkien").fields, [Field::Author]);
        assert_eq!(parse("fmt:pdf").extension.as_deref(), Some("pdf"));
    }

    #[test]
    fn topics_are_understood_and_bad_ones_ignored() {
        let t = parse("dune topic:fiction,comics");
        assert_eq!(t.topics, [Topic::Fiction, Topic::Comics]);
        assert_eq!(t.terms, "dune");
        // An unknown collection is dropped rather than failing a search that
        // is otherwise perfectly good.
        assert!(parse("dune topic:nonsense").topics.is_empty());
    }

    /// The whole reason unknown keys are passed through untouched.
    #[test]
    fn a_colon_that_is_not_a_tag_stays_in_the_query() {
        assert_eq!(parse("dune: messiah").terms, "dune: messiah");
        assert_eq!(parse("c:\\books").terms, "c:\\books");
        assert_eq!(
            parse("https://libgen.li/x").terms,
            "https://libgen.li/x",
            "a URL must survive intact"
        );
    }

    #[test]
    fn a_tag_with_no_value_is_ignored() {
        let t = parse("author: dune");
        assert_eq!(t.terms, "dune");
        assert!(t.fields.is_empty(), "half a tag is not an instruction");
    }

    #[test]
    fn repeated_tags_do_not_repeat_the_column() {
        let t = parse("author:le author:guin");
        assert_eq!(t.fields, [Field::Author]);
        assert_eq!(t.terms, "le guin");
    }

    #[test]
    fn multibyte_text_is_tokenised_by_character() {
        let t = parse("title:日本語 著者");
        assert_eq!(t.terms, "日本語 著者");
        assert_eq!(t.fields, [Field::Title]);
    }

    #[test]
    fn applying_to_a_query_respects_flags_already_set() {
        let mut q = SearchQuery::new("");
        q.extension = Some("pdf".into());
        q.fields.push(Field::Author);
        apply("dune ext:epub lang:english", &mut q);

        assert_eq!(q.terms, "dune");
        assert_eq!(
            q.extension.as_deref(),
            Some("pdf"),
            "an explicit --ext flag wins over a tag"
        );
        assert_eq!(q.language.as_deref(), Some("english"));
        assert_eq!(q.fields, [Field::Author], "flags are not duplicated");
    }

    #[test]
    fn an_empty_string_is_harmless() {
        let t = parse("   ");
        assert_eq!(t.terms, "");
        assert!(!t.is_tagged());
    }
}
