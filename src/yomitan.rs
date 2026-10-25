//! Asking the Yomitan API what a piece of Japanese text is.
//!
//! The app recognises the text on the screen but holds no dictionary of
//! its own. Yomitan, a browser extension, keeps the Japanese dictionaries
//! and runs a small HTTP server on the machine so that anything running
//! locally can ask it about a piece of text.
//!
//! Two things are wanted from it: how the text is broken up and how each
//! piece is read, and what the words in it mean.
//!
//! The API is HTTP/1.0. It sends no length for the body and closes the
//! connection when the answer is done, so the body is read to the end of
//! the connection unless a length is given.
//!
//! Everything here is blocking. The answers come in milliseconds, the
//! caller waits for them anyway, and a thread of its own would only make
//! the waiting harder to reason about.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The address the API is served on.
///
/// Yomitan serves the API on the machine, on a fixed port, so that
/// anything on the machine can ask the dictionaries about a piece of text.
pub const API: &str = "http://127.0.0.1:19633";

/// How long the API is given to answer before the call is given up on.
///
/// The API is on the same machine and answers in milliseconds, so this is
/// only a guard against a server that has stopped answering. The socket is
/// closed when the time is up, so a caller is never left waiting on a
/// connection that will not be used again.
const TIMEOUT: Duration = Duration::from_secs(5);

/// How much of the text the dictionary is asked to look at at once.
///
/// The API takes the length of the longest run of text it will scan. Ten
/// characters covers a word together with its reading and keeps the answer
/// small.
const SCAN_LENGTH: usize = 10;

/// A piece of the text, with the reading of the word in it, when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub reading: Option<String>,
}

/// A word, as the dictionary has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub term: String,
    pub reading: Option<String>,
    /// The meanings, one per line, already flattened to plain text.
    pub definitions: Vec<String>,
    /// The tags, such as the part of speech.
    pub tags: Vec<String>,
    /// Which dictionary it came from, when it says.
    pub dictionary: Option<String>,
}

/// Splits the text into the pieces it is made of, with their readings.
pub fn tokenize(text: &str) -> Result<Vec<Segment>> {
    let body = serde_json::json!({ "text": text, "scanLength": SCAN_LENGTH }).to_string();
    let answer = post("/tokenize", &body)?;
    let segments = parse_segments(&answer)?;
    tracing::debug!(
        "the API broke {} character(s) into {} piece(s)",
        text.chars().count(),
        segments.len()
    );
    Ok(segments)
}

/// The dictionary entries for a word.
pub fn entries(term: &str) -> Result<Vec<Entry>> {
    let body = serde_json::json!({ "term": term }).to_string();
    let answer = post("/termEntries", &body)?;
    let entries = parse_entries(&answer)?;
    tracing::debug!("the API has {} entry/entries for {term}", entries.len());
    Ok(entries)
}

/// The text with the readings marked: 人気（にんき）のおかず
///
/// The text is rebuilt from the pieces, so the reading of a word ends up
/// right after the word itself. A piece whose reading is the same as the
/// piece says nothing new, so it is left alone.
///
/// The text is returned as it is when there are no pieces, so that
/// something can always be shown even when the API did not answer.
pub fn with_readings(text: &str, segments: &[Segment]) -> String {
    if segments.is_empty() {
        return text.to_owned();
    }

    let mut out = String::new();
    for segment in segments {
        out.push_str(&segment.text);
        let Some(reading) = segment.reading.as_ref() else {
            continue;
        };
        if reading == &segment.text || reading.is_empty() {
            continue;
        }
        out.push('（');
        out.push_str(reading);
        out.push('）');
    }
    out
}

/// Sends a request to the API and returns the body of the answer.
fn post(path: &str, body: &str) -> Result<Vec<u8>> {
    let address = address()?;
    let deadline = Instant::now() + TIMEOUT;
    let mut stream = connect(address, deadline)?;
    send(&mut stream, address, path, body.as_bytes(), deadline)?;
    receive(stream, deadline)
}

/// The address of the API, as the socket address to connect to.
fn address() -> Result<SocketAddr> {
    let rest = API
        .strip_prefix("http://")
        .context("the API address should start with http://")?;
    rest.parse::<SocketAddr>()
        .with_context(|| format!("the API address {API} is not a host and a port"))
}

/// Opens the connection to the API.
fn connect(address: SocketAddr, deadline: Instant) -> Result<TcpStream> {
    match TcpStream::connect_timeout(&address, remaining(deadline)?) {
        Ok(stream) => Ok(stream),
        Err(err) => Err(unreachable_error(err, address)),
    }
}

/// The error to report when the API cannot be reached.
///
/// A refused connection means the extension is not running, and that is
/// worth saying plainly: nothing can be asked of the dictionaries until
/// the user has started it.
fn unreachable_error(error: std::io::Error, address: SocketAddr) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::ConnectionRefused {
        tracing::warn!("the Yomitan API is not running at {address}");
        return anyhow::anyhow!("the Yomitan API is not running at {address}");
    }
    anyhow::Error::new(error).context(format!("connect to the Yomitan API at {address}"))
}

/// The time the call has left, which is what the socket is given.
fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .context("the Yomitan API did not answer in time")
}

/// Writes the request to the socket.
fn send(
    stream: &mut TcpStream,
    address: SocketAddr,
    path: &str,
    body: &[u8],
    deadline: Instant,
) -> Result<()> {
    stream
        .set_write_timeout(Some(remaining(deadline)?))
        .context("set the write timeout")?;
    let request = build_request(address, path, body);
    stream
        .write_all(&request)
        .with_context(|| format!("send the request to {path}"))?;
    stream.flush().context("flush the request")?;
    Ok(())
}

/// The bytes of the request, built on their own.
///
/// They are put together without a socket so that the shape of the request
/// can be looked at without a server to send it to.
fn build_request(address: SocketAddr, path: &str, body: &[u8]) -> Vec<u8> {
    let head = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {address}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {length}\r\n\
         Connection: close\r\n\
         \r\n",
        length = body.len(),
    );
    let mut request = head.into_bytes();
    request.extend_from_slice(body);
    request
}

/// Reads the answer: the status line, the headers, and then the body.
///
/// The socket is owned here so that it is closed on every way out of this,
/// the timeout included.
fn receive(stream: TcpStream, deadline: Instant) -> Result<Vec<u8>> {
    stream
        .set_read_timeout(Some(remaining(deadline)?))
        .context("set the read timeout")?;
    let mut reader = BufReader::new(stream);

    read_status(&mut reader)?;
    let length = read_length(&mut reader)?;

    let mut body = Vec::new();
    match length {
        Some(length) => {
            let length = u64::try_from(length).context("the length of the body is too large")?;
            (&mut reader)
                .take(length)
                .read_to_end(&mut body)
                .context("read the body")?;
        }
        None => {
            // The API is HTTP/1.0 and closes the connection when the answer
            // is done, so the body is read to the end of the connection.
            reader.read_to_end(&mut body).context("read the body")?;
        }
    }
    Ok(body)
}

/// Reads the status line and makes sure the answer was a success.
fn read_status(reader: &mut BufReader<TcpStream>) -> Result<()> {
    let mut line = String::new();
    if reader.read_line(&mut line).context("read the status line")? == 0 {
        bail!("the Yomitan API closed the connection without answering");
    }

    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    let status = parts.next().unwrap_or_default();
    let reason = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        bail!("the Yomitan API did not answer with HTTP: {line:?}");
    }

    let status: u16 = status
        .parse()
        .with_context(|| format!("the status of the answer is {status:?}"))?;
    if !(200..300).contains(&status) {
        bail!("the Yomitan API answered {status} {reason}");
    }
    Ok(())
}

/// Reads the headers and returns the length of the body, when it says.
fn read_length(reader: &mut BufReader<TcpStream>) -> Result<Option<usize>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).context("read the headers")? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("content-length") {
            continue;
        }
        let value = value.trim();
        length = Some(
            value
                .parse::<usize>()
                .with_context(|| format!("the length of the body is {value:?}"))?,
        );
    }
    Ok(length)
}

/// One piece of the text as the dictionary sees it.
///
/// The answer carries the matches of the piece as well, under `headwords`.
/// They are of no use here, and `serde` leaves out the fields it does not
/// know, so they do not have to be listed.
#[derive(Debug, Deserialize)]
struct Piece {
    #[serde(default)]
    text: String,
    #[serde(default)]
    reading: String,
}

/// One answer of the tokenise endpoint: the pieces of every text that was
/// scanned.
#[derive(Debug, Deserialize)]
struct TokenizeAnswer {
    #[serde(default)]
    content: Vec<Vec<Piece>>,
}

/// Reads the pieces of the text out of the answer of the API.
fn parse_segments(body: &[u8]) -> Result<Vec<Segment>> {
    let answers: Vec<TokenizeAnswer> =
        serde_json::from_slice(body).context("read the answer of the tokenise endpoint")?;
    Ok(answers
        .into_iter()
        .flat_map(|answer| answer.content)
        .flatten()
        .map(|piece| Segment {
            text: piece.text,
            // The API leaves the reading empty when there is none.
            reading: (!piece.reading.is_empty()).then_some(piece.reading),
        })
        .collect())
}

/// The answer of the dictionary endpoint.
#[derive(Debug, Deserialize)]
struct TermEntriesAnswer {
    #[serde(default, rename = "dictionaryEntries")]
    entries: Vec<SourceEntry>,
}

/// One entry of a dictionary, with the words it was found under.
#[derive(Debug, Deserialize)]
struct SourceEntry {
    #[serde(default)]
    headwords: Vec<Headword>,
    #[serde(default)]
    definitions: Vec<Definition>,
}

/// A word as the dictionary spells it.
#[derive(Debug, Deserialize)]
struct Headword {
    #[serde(default)]
    term: String,
    #[serde(default)]
    reading: String,
    #[serde(default)]
    tags: Vec<WordTag>,
}

/// A label the dictionary puts on a word, such as the part of speech.
#[derive(Debug, Deserialize)]
struct WordTag {
    #[serde(default)]
    name: String,
}

/// One meaning of a word, as one dictionary has it.
#[derive(Debug, Deserialize)]
struct Definition {
    #[serde(default, rename = "dictionaryAlias")]
    alias: String,
    #[serde(default)]
    tags: Vec<WordTag>,
    /// The meaning itself, which is a tree of nodes or plain text.
    #[serde(default)]
    entries: Vec<NodeContent>,
}

/// A node of the tree a meaning is made of.
#[derive(Debug, Deserialize)]
struct ContentNode {
    /// The name of the element, such as `ul` or `span`. The answer also
    /// wraps the tree in elements that have no name.
    #[serde(default)]
    tag: Option<String>,
    /// The label of the element, which says what word class it marks.
    #[serde(default)]
    title: Option<String>,
    /// What the element holds.
    #[serde(default)]
    content: Option<NodeContent>,
}

/// What a node holds: text, more nodes, or one node.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NodeContent {
    /// The text of the node.
    Text(String),
    /// More nodes, which is how a list holds its items. The items may be
    /// text as well, which is how a ruby holds the character it belongs to.
    Nodes(Vec<NodeContent>),
    /// One node, which is how the answer wraps the tree.
    Node(Box<ContentNode>),
    /// Something this client does not know, which is left out rather than
    /// failing the whole lookup.
    Unknown(serde_json::Value),
}

/// Turns what the dictionaries say into the entries the app shows.
fn parse_entries(body: &[u8]) -> Result<Vec<Entry>> {
    let answer: TermEntriesAnswer =
        serde_json::from_slice(body).context("read the answer of the dictionary endpoint")?;
    Ok(answer.entries.into_iter().map(build_entry).collect())
}

/// Builds one entry out of what one dictionary said about a word.
fn build_entry(source: SourceEntry) -> Entry {
    let headword = source.headwords.first();
    let term = headword.map(|word| word.term.clone()).unwrap_or_default();
    let reading = headword
        .map(|word| word.reading.clone())
        .filter(|reading| !reading.is_empty());

    let mut tags = Vec::new();
    for headword in &source.headwords {
        for tag in &headword.tags {
            push_tag(&mut tags, &tag.name);
        }
    }
    for definition in &source.definitions {
        for tag in &definition.tags {
            push_tag(&mut tags, &tag.name);
        }
    }

    let mut definitions = Vec::new();
    for definition in &source.definitions {
        definitions.extend(flatten(&definition.entries));
    }

    // One entry can carry meanings from more than one dictionary, and only
    // one of them can be named here.
    let dictionary = source
        .definitions
        .iter()
        .map(|definition| definition.alias.as_str())
        .find(|alias| !alias.is_empty())
        .map(str::to_owned);

    Entry {
        term,
        reading,
        definitions,
        tags,
        dictionary,
    }
}

/// Adds a tag to the list, keeping the list free of repeats.
fn push_tag(tags: &mut Vec<String>, name: &str) {
    if name.is_empty() || tags.iter().any(|tag| tag == name) {
        return;
    }
    tags.push(name.to_owned());
}

/// Flattens the tree of a meaning into the lines of plain text.
fn flatten(nodes: &[NodeContent]) -> Vec<String> {
    let mut flattener = Flattener::default();
    for node in nodes {
        flattener.walk_content(node);
    }
    flattener.finish()
}

/// Collects the text of a tree of nodes into lines, one line per meaning.
#[derive(Debug, Default)]
struct Flattener {
    lines: Vec<String>,
    current: String,
}

impl Flattener {
    /// Adds the text of a node to the line being built.
    fn text(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.needs_space(text) {
            self.current.push(' ');
        }
        self.current.push_str(text);
    }

    /// Whether a space belongs between what has been written and the text
    /// that follows it.
    ///
    /// Japanese is written without spaces, so a space between two pieces
    /// of Japanese would break up a word that is written as one.
    fn needs_space(&self, text: &str) -> bool {
        if self.current.is_empty() || self.current.ends_with(' ') {
            return false;
        }
        let Some(previous) = self.current.chars().next_back() else {
            return false;
        };
        let Some(next) = text.chars().next() else {
            return false;
        };
        previous.is_ascii() || next.is_ascii()
    }

    /// Ends the line being built and starts a new one.
    fn break_line(&mut self) {
        let line = self.current.trim();
        if !line.is_empty() {
            self.lines.push(line.to_owned());
        }
        self.current.clear();
    }

    /// Walks what a node holds.
    fn walk_content(&mut self, content: &NodeContent) {
        match content {
            NodeContent::Text(text) => self.text(text),
            NodeContent::Nodes(nodes) => {
                for node in nodes {
                    self.walk_content(node);
                }
            }
            NodeContent::Node(node) => self.walk(node),
            NodeContent::Unknown(value) => {
                tracing::debug!("the API sent a kind of content this client does not know: {value}");
            }
        }
    }

    /// Walks a node and everything under it.
    fn walk(&mut self, node: &ContentNode) {
        match node.tag.as_deref() {
            // A list item is a meaning of its own: inside an `ol` these are
            // the numbered senses of the word, and inside a `ul` they are
            // the bullets of a gloss. Either way they would otherwise run
            // into each other.
            Some("li") => self.break_line(),
            // A ruby reading is how the character next to it is pronounced,
            // which is not part of the meaning.
            Some("rt") => return,
            _ => {}
        }

        if let Some(title) = node.title.as_deref().filter(|title| !title.trim().is_empty()) {
            // The title says what word class the node marks, and goes
            // in front of the text it marks.
            self.current.push_str(" [");
            self.current.push_str(title.trim());
            self.current.push_str("] ");
        }

        if let Some(content) = &node.content {
            self.walk_content(content);
        }

        if matches!(node.tag.as_deref(), Some("ul" | "ol")) {
            // What follows a list is not part of its last item.
            self.break_line();
        }
    }

    /// The lines that were built.
    fn finish(mut self) -> Vec<String> {
        self.break_line();
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A piece of the text, as the API would give it.
    fn segment(text: &str, reading: Option<&str>) -> Segment {
        Segment {
            text: text.to_owned(),
            reading: reading.map(str::to_owned),
        }
    }

    /// A word is marked with its reading.
    #[test]
    fn a_word_is_marked_with_its_reading() {
        let segments = [
            segment("人気", Some("にんき")),
            segment("の", None),
            segment("おかず", None),
        ];

        let text = with_readings("人気のおかず", &segments);

        assert_eq!(text, "人気（にんき）のおかず");
    }

    /// A piece without a reading is left as it is.
    #[test]
    fn a_word_without_a_reading_is_left_alone() {
        let segments = [segment("おかず", None)];

        let text = with_readings("おかず", &segments);

        assert_eq!(text, "おかず");
    }

    /// A reading that is the same as the word says nothing new, so it is not
    /// worth the space it takes.
    #[test]
    fn a_word_that_reads_as_itself_is_left_alone() {
        let segments = [segment("テレビ", Some("テレビ"))];

        let text = with_readings("テレビ", &segments);

        assert_eq!(text, "テレビ");
    }

    /// When the API did not answer there are no pieces, and the text that
    /// was recognised is all there is to show.
    #[test]
    fn the_text_is_shown_as_it_is_when_there_are_no_pieces() {
        let text = with_readings("人気のおかず", &[]);

        assert_eq!(text, "人気のおかず");
    }

    /// The answer of the tokenise endpoint, with the matches of the piece
    /// left in, which are of no use here.
    const TOKENIZE_ANSWER: &str = r#"[
        {
            "id": "scan",
            "source": "scanning-parser",
            "dictionary": null,
            "index": 0,
            "content": [
                [
                    {
                        "text": "人気",
                        "reading": "にんき",
                        "headwords": [
                            [
                                {
                                    "term": "人気",
                                    "reading": "にんき",
                                    "sources": [
                                        {
                                            "originalText": "人気",
                                            "transformedText": "人気",
                                            "matchType": "exact",
                                            "isPrimary": true
                                        }
                                    ]
                                }
                            ]
                        ]
                    }
                ],
                [{ "text": "の", "reading": "", "headwords": [] }],
                [{ "text": "おかず", "reading": "", "headwords": [] }]
            ]
        }
    ]"#;

    #[test]
    fn the_answer_of_the_tokenise_endpoint_is_read() {
        let segments = parse_segments(TOKENIZE_ANSWER.as_bytes()).expect("the answer is read");

        assert_eq!(
            segments,
            vec![
                segment("人気", Some("にんき")),
                segment("の", None),
                segment("おかず", None),
            ]
        );
    }

    /// The answer of the dictionary endpoint, with the tree of one meaning
    /// in it.
    const TERM_ENTRIES_ANSWER: &str = r#"{
        "dictionaryEntries": [
            {
                "type": "term",
                "isPrimary": true,
                "headwords": [
                    {
                        "index": 0,
                        "term": "人気",
                        "reading": "にんき",
                        "tags": [{ "name": "n", "category": "partOfSpeech" }]
                    }
                ],
                "definitions": [
                    {
                        "index": 0,
                        "dictionaryAlias": "Jitendex.org",
                        "tags": [{ "name": "★", "category": "popular" }],
                        "entries": [
                            {
                                "type": "structured-content",
                                "content": {
                                    "tag": "ol",
                                    "content": [
                                        {
                                            "tag": "li",
                                            "content": [
                                                {
                                                    "tag": "span",
                                                    "title": "noun",
                                                    "content": "noun"
                                                },
                                                {
                                                    "tag": "ol",
                                                    "content": [
                                                        {
                                                            "tag": "li",
                                                            "content": {
                                                                "tag": "ul",
                                                                "content": [
                                                                    { "tag": "li", "content": "popularity" },
                                                                    { "tag": "li", "content": "public favor" }
                                                                ]
                                                            }
                                                        }
                                                    ]
                                                }
                                            ]
                                        },
                                        { "tag": "li", "content": "condition (e.g. market)" }
                                    ]
                                }
                            }
                        ]
                    }
                ]
            }
        ],
        "originalTextLength": 8,
        "index": 0
    }"#;

    #[test]
    fn the_answer_of_the_dictionary_endpoint_is_read() {
        let entries = parse_entries(TERM_ENTRIES_ANSWER.as_bytes()).expect("the answer is read");

        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.term, "人気");
        assert_eq!(entry.reading, Some("にんき".to_owned()));
        assert_eq!(entry.dictionary, Some("Jitendex.org".to_owned()));
        assert_eq!(entry.tags, vec!["n".to_owned(), "★".to_owned()]);
        assert_eq!(
            entry.definitions,
            vec![
                "[noun] noun".to_owned(),
                "popularity".to_owned(),
                "public favor".to_owned(),
                "condition (e.g. market)".to_owned(),
            ]
        );
    }

    /// The API is asked with a plain JSON body, and the request has to say
    /// how long that body is so that the answer can be read.
    #[test]
    fn the_request_is_built_the_way_the_api_expects() {
        let address: SocketAddr = "127.0.0.1:19633".parse().expect("the address of the API");
        let body = r#"{"scanLength":10,"text":"人気"}"#;

        let request = build_request(address, "/tokenize", body.as_bytes());

        let end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("the head ends with an empty line");
        let head = String::from_utf8(request[..end].to_vec()).expect("the head is text");
        let sent = &request[end + 4..];

        assert!(head.starts_with("POST /tokenize HTTP/1.1\r\n"), "got {head:?}");
        assert!(head.contains("\r\nHost: 127.0.0.1:19633\r\n"), "got {head:?}");
        assert!(
            head.contains("\r\nContent-Type: application/json\r\n"),
            "got {head:?}"
        );
        assert!(
            head.contains(&format!("\r\nContent-Length: {}\r\n", body.len())),
            "got {head:?}"
        );
        assert_eq!(sent, body.as_bytes());
    }

    /// A word that is only known to one dictionary still has to be read.
    #[test]
    fn an_entry_without_a_dictionary_is_read() {
        let answer = r#"{
            "dictionaryEntries": [
                {
                    "headwords": [{ "term": " popularity", "reading": "" }],
                    "definitions": []
                }
            ]
        }"#;

        let entries = parse_entries(answer.as_bytes()).expect("the answer is read");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].term, " popularity");
        assert_eq!(entries[0].reading, None);
        assert_eq!(entries[0].dictionary, None);
        assert!(entries[0].definitions.is_empty());
    }

    /// Japanese is written without spaces, so a space between two pieces
    /// of it would break up a word that is written as one.
    #[test]
    fn japanese_pieces_are_not_put_apart() {
        let answer = r#"{
            "dictionaryEntries": [
                {
                    "headwords": [{ "term": "人気", "reading": "にんき" }],
                    "definitions": [
                        {
                            "dictionaryAlias": "Jitendex.org",
                            "entries": [
                                {
                                    "type": "structured-content",
                                    "content": {
                                        "tag": "ul",
                                        "content": [
                                            { "tag": "li", "content": "人気がでて" },
                                            {
                                                "tag": "li",
                                                "content": [
                                                    {
                                                        "tag": "ruby",
                                                        "content": [
                                                            "人",
                                                            { "tag": "rt", "content": "にん" }
                                                        ]
                                                    },
                                                    {
                                                        "tag": "ruby",
                                                        "content": [
                                                            "気",
                                                            { "tag": "rt", "content": "き" }
                                                        ]
                                                    }
                                                ]
                                            },
                                            {
                                                "tag": "li",
                                                "content": [
                                                    { "tag": "div", "content": "Note" },
                                                    { "tag": "div", "content": "indicates popularity" }
                                                ]
                                            }
                                        ]
                                    }
                                }
                            ]
                        }
                    ]
                }
            ]
        }"#;

        let entries = parse_entries(answer.as_bytes()).expect("the answer is read");

        assert_eq!(
            entries[0].definitions,
            vec![
                "人気がでて".to_owned(),
                "人気".to_owned(),
                "Note indicates popularity".to_owned(),
            ]
        );
    }

    /// Some dictionaries answer with plain text instead of a tree of
    /// nodes, and that text is the meaning.
    #[test]
    fn a_meaning_that_is_plain_text_is_read() {
        let answer = r#"{
            "dictionaryEntries": [
                {
                    "headwords": [{ "term": "人", "reading": "じん" }],
                    "definitions": [
                        { "dictionaryAlias": "JMdict", "entries": ["人（★）"] }
                    ]
                }
            ]
        }"#;

        let entries = parse_entries(answer.as_bytes()).expect("the answer is read");

        assert_eq!(entries[0].definitions, vec!["人（★）".to_owned()]);
    }

    /// The extension has to be running for anything to be asked of it, and
    /// that is worth saying when it is not.
    #[test]
    fn a_refused_connection_says_the_api_is_not_running() {
        let address: SocketAddr = "127.0.0.1:19633".parse().expect("the address of the API");
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);

        let error = unreachable_error(refused, address);

        assert!(
            error.to_string().contains("not running"),
            "the error should say the API is not running, got {error}"
        );
    }

    /// A failure that is not a refusal says what could not be reached.
    #[test]
    fn another_failure_says_where_the_api_could_not_be_reached() {
        let address: SocketAddr = "127.0.0.1:19633".parse().expect("the address of the API");
        let broken = std::io::Error::other("no route to host");

        let error = unreachable_error(broken, address);

        assert!(
            error.to_string().contains("127.0.0.1:19633"),
            "the error should name the address, got {error}"
        );
    }
}
