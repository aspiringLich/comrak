mod table;
mod autolink;

use std::cell::RefCell;
use std::cmp::min;
use std::collections::HashMap;
use std::mem;

use typed_arena::Arena;

use arena_tree::Node;
use ctype::{isspace, isdigit};
use nodes::{NodeValue, Ast, NodeCodeBlock, NodeHeading, NodeList, ListType, ListDelimType,
            NodeHtmlBlock, make_block, AstNode};
use scanners;
use strings;
use entity;
use inlines;
use nodes;

const TAB_STOP: usize = 4;
const CODE_INDENT: usize = 4;
pub const MAXBACKTICKS: usize = 80;
pub const MAX_LINK_LABEL_LENGTH: usize = 1000;

/// Parse a Markdown document to an AST.
///
/// See the documentation of the crate root for an example.
pub fn parse_document<'a>(arena: &'a Arena<AstNode<'a>>,
                          buffer: &str,
                          options: &ComrakOptions)
                          -> &'a AstNode<'a> {
    let root: &'a AstNode<'a> = arena.alloc(Node::new(RefCell::new(Ast {
        value: NodeValue::Document,
        content: String::new(),
        start_line: 0,
        start_column: 0,
        end_line: 0,
        end_column: 0,
        open: true,
        last_line_blank: false,
    })));
    let mut parser = Parser::new(arena, root, options);
    parser.feed(buffer, true);
    parser.finish()
}

pub struct Parser<'a, 'o> {
    arena: &'a Arena<AstNode<'a>>,
    refmap: HashMap<String, Reference>,
    root: &'a AstNode<'a>,
    current: &'a AstNode<'a>,
    line_number: u32,
    offset: usize,
    column: usize,
    first_nonspace: usize,
    first_nonspace_column: usize,
    indent: usize,
    blank: bool,
    partially_consumed_tab: bool,
    last_line_length: usize,
    linebuf: String,
    last_buffer_ended_with_cr: bool,
    options: &'o ComrakOptions,
}

#[derive(Default)]
/// Options for both parser and formatter functions.
pub struct ComrakOptions {
    /// [Soft line breaks](http://spec.commonmark.org/0.27/#soft-line-breaks) in the input
    /// translate into hard line breaks in the output.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// assert_eq!(markdown_to_html("Hello.\nWorld.\n", &options),
    ///            "<p>Hello.\nWorld.</p>\n");
    ///
    /// options.hardbreaks = true;
    /// assert_eq!(markdown_to_html("Hello.\nWorld.\n", &options),
    ///            "<p>Hello.<br />\nWorld.</p>\n");
    /// ```
    pub hardbreaks: bool,

    /// GitHub-style `<pre lang="xyz">` is used for fenced code blocks with info tags.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// assert_eq!(markdown_to_html("``` rust\nfn hello();\n```\n", &options),
    ///            "<pre><code class=\"language-rust\">fn hello();\n</code></pre>\n");
    ///
    /// options.github_pre_lang = true;
    /// assert_eq!(markdown_to_html("``` rust\nfn hello();\n```\n", &options),
    ///            "<pre lang=\"rust\"><code>fn hello();\n</code></pre>\n");
    /// ```
    pub github_pre_lang: bool,

    /// The wrap column when outputting CommonMark.
    ///
    /// ```
    /// # extern crate typed_arena;
    /// # extern crate comrak;
    /// # use comrak::{parse_document, ComrakOptions, format_commonmark};
    /// # fn main() {
    /// # let arena = typed_arena::Arena::new();
    /// let mut options = ComrakOptions::default();
    /// let node = parse_document(&arena, "hello hello hello hello hello hello", &options);
    /// assert_eq!(format_commonmark(node, &options),
    ///            "hello hello hello hello hello hello\n");
    ///
    /// options.width = 20;
    /// assert_eq!(format_commonmark(node, &options),
    ///            "hello hello hello\nhello hello hello\n");
    /// # }
    /// ```
    pub width: usize,

    /// Enables the
    /// [strikethrough extension](https://github.github.com/gfm/#strikethrough-extension-)
    /// from the GFM spec.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.ext_strikethrough = true;
    /// assert_eq!(markdown_to_html("Hello ~world~ there.\n", &options),
    ///            "<p>Hello <del>world</del> there.</p>\n");
    /// ```
    pub ext_strikethrough: bool,

    /// Enables the
    /// [tagfilter extension](https://github.github.com/gfm/#disallowed-raw-html-extension-)
    /// from the GFM spec.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.ext_tagfilter = true;
    /// assert_eq!(markdown_to_html("Hello <xmp>.\n\n<xmp>", &options),
    ///            "<p>Hello &lt;xmp>.</p>\n&lt;xmp>\n");
    /// ```
    pub ext_tagfilter: bool,

    /// Enables the [table extension](https://github.github.com/gfm/#tables-extension-)
    /// from the GFM spec.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.ext_table = true;
    /// assert_eq!(markdown_to_html("| a | b |\n|---|---|\n| c | d |\n", &options),
    ///            "<table>\n<thead>\n<tr>\n<th>a</th>\n<th>b</th>\n</tr>\n</thead>\n\
    ///             <tbody>\n<tr>\n<td>c</td>\n<td>d</td>\n</tr></tbody></table>\n");
    /// ```
    pub ext_table: bool,
    /// Enables the [autolink extension](https://github.github.com/gfm/#autolinks-extension-)
    /// from the GFM spec.
    ///
    /// ```
    /// # use comrak::{markdown_to_html, ComrakOptions};
    /// let mut options = ComrakOptions::default();
    /// options.ext_autolink = true;
    /// assert_eq!(markdown_to_html("Hello www.github.com.\n", &options),
    ///            "<p>Hello <a href=\"http://www.github.com\">www.github.com</a>.</p>\n");
    /// ```
    pub ext_autolink: bool,
}


#[derive(Clone)]
pub struct Reference {
    pub url: String,
    pub title: String,
}

impl<'a, 'o> Parser<'a, 'o> {
    pub fn new(arena: &'a Arena<AstNode<'a>>,
               root: &'a AstNode<'a>,
               options: &'o ComrakOptions)
               -> Parser<'a, 'o> {
        Parser {
            arena: arena,
            refmap: HashMap::new(),
            root: root,
            current: root,
            line_number: 0,
            offset: 0,
            column: 0,
            first_nonspace: 0,
            first_nonspace_column: 0,
            indent: 0,
            blank: false,
            partially_consumed_tab: false,
            last_line_length: 0,
            linebuf: String::new(),
            last_buffer_ended_with_cr: false,
            options: options,
        }
    }

    pub fn feed(&mut self, mut buffer: &str, eof: bool) {
        if self.last_buffer_ended_with_cr && buffer.as_bytes()[0] == '\n' as u8 {
            buffer = &buffer[1..];
        }
        self.last_buffer_ended_with_cr = false;

        while buffer.len() > 0 {
            let mut process = false;
            let mut eol = 0;
            while eol < buffer.len() {
                if strings::is_line_end_char(&buffer.as_bytes()[eol]) {
                    process = true;
                    break;
                }
                if buffer.as_bytes()[eol] == 0 {
                    break;
                }
                eol += 1;
            }

            if eol >= buffer.len() && eof {
                process = true;
            }

            if process {
                if self.linebuf.len() > 0 {
                    self.linebuf += &buffer[0..eol];
                    let linebuf = mem::replace(&mut self.linebuf, String::new());
                    self.process_line(&linebuf);
                } else {
                    self.process_line(&buffer[0..eol]);
                }
            } else {
                if eol < buffer.len() && buffer.as_bytes()[eol] == '\0' as u8 {
                    self.linebuf += &buffer[0..eol];
                    self.linebuf.push('\u{fffd}');
                    eol += 1;
                } else {
                    self.linebuf += &buffer[0..eol];
                }
            }

            buffer = &buffer[eol..];
            if buffer.len() > 0 && buffer.as_bytes()[0] == '\r' as u8 {
                buffer = &buffer[1..];
                if buffer.len() == 0 {
                    self.last_buffer_ended_with_cr = true;
                }
            }
            if buffer.len() > 0 && buffer.as_bytes()[0] == '\n' as u8 {
                buffer = &buffer[1..];
            }
        }
    }

    fn find_first_nonspace(&mut self, line: &mut String) {
        self.first_nonspace = self.offset;
        self.first_nonspace_column = self.column;
        let mut chars_to_tab = TAB_STOP - (self.column % TAB_STOP);

        loop {
            if self.first_nonspace >= line.len() {
                break;
            }
            match line.as_bytes()[self.first_nonspace] {
                32 => {
                    self.first_nonspace += 1;
                    self.first_nonspace_column += 1;
                    chars_to_tab -= 1;
                    if chars_to_tab == 0 {
                        chars_to_tab = TAB_STOP;
                    }
                }
                9 => {
                    self.first_nonspace += 1;
                    self.first_nonspace_column += chars_to_tab;
                    chars_to_tab = TAB_STOP;
                }
                _ => break,
            }
        }

        self.indent = self.first_nonspace_column - self.column;
        self.blank = self.first_nonspace < line.len() &&
                     strings::is_line_end_char(&line.as_bytes()[self.first_nonspace]);
    }

    fn process_line(&mut self, buffer: &str) {
        let mut line: String = buffer.into();
        if line.len() == 0 || !strings::is_line_end_char(&line.as_bytes().last().unwrap()) {
            line.push('\n');
        }

        self.offset = 0;
        self.column = 0;
        self.blank = false;
        self.partially_consumed_tab = false;

        if self.line_number == 0 && line.len() >= 3 && line.chars().next().unwrap() == '\u{feff}' {
            self.offset += 3;
        }

        self.line_number += 1;

        let mut all_matched = true;
        if let Some(last_matched_container) = self.check_open_blocks(&mut line, &mut all_matched) {
            let mut container = last_matched_container;
            let current = self.current;
            self.open_new_blocks(&mut container, &mut line, all_matched);

            if current.same_node(self.current) {
                self.add_text_to_container(container, last_matched_container, &mut line);
            }
        }

        self.last_line_length = line.len();
        if self.last_line_length > 0 && line.as_bytes()[self.last_line_length - 1] == '\n' as u8 {
            self.last_line_length -= 1;
        }
        if self.last_line_length > 0 && line.as_bytes()[self.last_line_length - 1] == '\r' as u8 {
            self.last_line_length -= 1;
        }
    }

    fn check_open_blocks(&mut self,
                         line: &mut String,
                         all_matched: &mut bool)
                         -> Option<&'a AstNode<'a>> {
        let mut should_continue = true;
        *all_matched = false;
        let mut container = self.root;

        'done: loop {
            while nodes::last_child_is_open(container) {
                container = container.last_child().unwrap();
                let ast = &mut *container.data.borrow_mut();

                self.find_first_nonspace(line);

                match ast.value {
                    NodeValue::BlockQuote => {
                        if !self.parse_block_quote_prefix(line) {
                            break 'done;
                        }
                    }
                    NodeValue::Item(ref nl) => {
                        if !self.parse_node_item_prefix(line, container, nl) {
                            break 'done;
                        }
                    }
                    NodeValue::CodeBlock(..) => {
                        if !self.parse_code_block_prefix(
                            line, container, ast, &mut should_continue) {
                            break 'done;
                        }
                    }
                    NodeValue::Heading(..) => {
                        break 'done;
                    }
                    NodeValue::HtmlBlock(ref nhb) => {
                        if !self.parse_html_block_prefix(nhb.block_type) {
                            break 'done;
                        }
                    }
                    NodeValue::Paragraph => {
                        if self.blank {
                            break 'done;
                        }
                    }
                    NodeValue::Table(..) => {
                        if !table::matches(&line[self.first_nonspace..]) {
                            break 'done;
                        }
                        continue;
                    }
                    NodeValue::TableRow(..) => {
                        break 'done;
                    }
                    NodeValue::TableCell => {
                        break 'done;
                    }
                    _ => {}
                }
            }

            *all_matched = true;
            break 'done;
        }

        if !*all_matched {
            container = container.parent().unwrap();
        }

        if !should_continue {
            None
        } else {
            Some(container)
        }
    }

    fn open_new_blocks(&mut self,
                       container: &mut &'a AstNode<'a>,
                       line: &mut String,
                       all_matched: bool) {
        let mut matched: usize = 0;
        let mut nl: NodeList = NodeList::default();
        let mut sc: scanners::SetextChar = scanners::SetextChar::Equals;
        let mut maybe_lazy = match &self.current.data.borrow().value {
            &NodeValue::Paragraph => true,
            _ => false,
        };

        while match &container.data.borrow().value {
            &NodeValue::CodeBlock(..) |
            &NodeValue::HtmlBlock(..) => false,
            _ => true,
        } {
            self.find_first_nonspace(line);
            let indented = self.indent >= CODE_INDENT;

            if !indented && line.as_bytes()[self.first_nonspace] == '>' as u8 {
                let blockquote_startpos = self.first_nonspace;
                let offset = self.first_nonspace + 1 - self.offset;
                self.advance_offset(line, offset, false);
                if strings::is_space_or_tab(&line.as_bytes()[self.offset]) {
                    self.advance_offset(line, 1, true);
                }
                *container =
                    self.add_child(*container, NodeValue::BlockQuote, blockquote_startpos + 1);
            } else if !indented &&
                      unwrap_into(scanners::atx_heading_start(&line[self.first_nonspace..]),
                                  &mut matched) {
                let heading_startpos = self.first_nonspace;
                let offset = self.offset;
                self.advance_offset(line, heading_startpos + matched - offset, false);
                *container = self.add_child(*container,
                                            NodeValue::Heading(NodeHeading::default()),
                                            heading_startpos + 1);

                let mut hashpos =
                    line[self.first_nonspace..].bytes().position(|c| c == '#' as u8).unwrap() +
                    self.first_nonspace;
                let mut level = 0;
                while line.as_bytes()[hashpos] == '#' as u8 {
                    level += 1;
                    hashpos += 1;
                }

                container.data.borrow_mut().value = NodeValue::Heading(NodeHeading {
                    level: level,
                    setext: false,
                });

            } else if !indented &&
                      unwrap_into(scanners::open_code_fence(&line[self.first_nonspace..]),
                                  &mut matched) {
                let first_nonspace = self.first_nonspace;
                let offset = self.offset;
                let ncb = NodeCodeBlock {
                    fenced: true,
                    fence_char: line.as_bytes()[first_nonspace],
                    fence_length: matched,
                    fence_offset: first_nonspace - offset,
                    info: String::new(),
                    literal: String::new(),
                };
                *container =
                    self.add_child(*container, NodeValue::CodeBlock(ncb), first_nonspace + 1);
                self.advance_offset(line, first_nonspace + matched - offset, false);
            } else if !indented &&
                      (unwrap_into(scanners::html_block_start(&line[self.first_nonspace..]),
                                   &mut matched) ||
                       match &container.data.borrow().value {
                &NodeValue::Paragraph => false,
                _ => {
                    unwrap_into(scanners::html_block_start_7(&line[self.first_nonspace..]),
                                &mut matched)
                }
            }) {
                let offset = self.first_nonspace + 1;
                let nhb = NodeHtmlBlock {
                    block_type: matched as u8,
                    literal: String::new(),
                };

                *container = self.add_child(*container, NodeValue::HtmlBlock(nhb), offset);
            } else if !indented &&
                      match &container.data.borrow().value {
                &NodeValue::Paragraph => {
                    unwrap_into(scanners::setext_heading_line(&line[self.first_nonspace..]),
                                &mut sc)
                }
                _ => false,
            } {
                container.data.borrow_mut().value = NodeValue::Heading(NodeHeading {
                    level: match sc {
                        scanners::SetextChar::Equals => 1,
                        scanners::SetextChar::Hyphen => 2,
                    },
                    setext: true,
                });
                let adv = line.len() - 1 - self.offset;
                self.advance_offset(line, adv, false);
            } else if !indented &&
                      match (&container.data.borrow().value, all_matched) {
                (&NodeValue::Paragraph, false) => false,
                _ => {
                    unwrap_into(scanners::thematic_break(&line[self.first_nonspace..]),
                                &mut matched)
                }
            } {
                let offset = self.first_nonspace + 1;
                *container = self.add_child(*container, NodeValue::ThematicBreak, offset);
                let adv = line.len() - 1 - self.offset;
                self.advance_offset(line, adv, false);
            } else if (!indented ||
                       match &container.data.borrow().value {
                &NodeValue::List(..) => true,
                _ => false,
            }) &&
                      unwrap_into_2(parse_list_marker(line,
                                                      self.first_nonspace,
                                                      match &container.data.borrow().value {
                                                          &NodeValue::Paragraph => true,
                                                          _ => false,
                                                      }),
                                    &mut matched,
                                    &mut nl) {
                let offset = self.first_nonspace + matched - self.offset;
                self.advance_offset(line, offset, false);
                let (save_partially_consumed_tab, save_offset, save_column) =
                    (self.partially_consumed_tab, self.offset, self.column);

                while self.column - save_column <= 5 &&
                      strings::is_space_or_tab(&line.as_bytes()[self.offset]) {
                    self.advance_offset(line, 1, true);
                }

                let i = self.column - save_column;
                if i >= 5 || i < 1 || strings::is_line_end_char(&line.as_bytes()[self.offset]) {
                    nl.padding = matched + 1;
                    self.offset = save_offset;
                    self.column = save_column;
                    self.partially_consumed_tab = save_partially_consumed_tab;
                    if i > 0 {
                        self.advance_offset(line, 1, true);
                    }
                } else {
                    nl.padding = matched + i;
                }

                nl.marker_offset = self.indent;

                let offset = self.first_nonspace + 1;
                if match &container.data.borrow().value {
                    &NodeValue::List(ref mnl) => !lists_match(&nl, mnl),
                    _ => true,
                } {
                    *container = self.add_child(*container, NodeValue::List(nl), offset);
                }

                let offset = self.first_nonspace + 1;
                *container = self.add_child(*container, NodeValue::Item(nl), offset);
            } else if indented && !maybe_lazy && !self.blank {
                self.advance_offset(line, CODE_INDENT, true);
                let ncb = NodeCodeBlock {
                    fenced: false,
                    fence_char: 0,
                    fence_length: 0,
                    fence_offset: 0,
                    info: String::new(),
                    literal: String::new(),
                };
                let offset = self.offset + 1;
                *container = self.add_child(*container, NodeValue::CodeBlock(ncb), offset);
            } else {
                let mut new_container = None;

                if !indented && self.options.ext_table {
                    new_container = table::try_opening_block(self, *container, line);
                }

                match new_container {
                    Some((new_container, replace)) => {
                        if replace {
                            container.insert_after(new_container);
                            container.detach();
                            *container = new_container;
                        } else {
                            *container = new_container;
                        }
                    }
                    _ => break,
                }
            }

            if container.data.borrow().value.accepts_lines() {
                break;
            }

            maybe_lazy = false;
        }
    }

    fn advance_offset(&mut self, line: &str, mut count: usize, columns: bool) {
        while count > 0 {
            match line.as_bytes()[self.offset] {
                9 => {
                    let chars_to_tab = TAB_STOP - (self.column % TAB_STOP);
                    if columns {
                        self.partially_consumed_tab = chars_to_tab > count;
                        let chars_to_advance = min(count, chars_to_tab);
                        self.column += chars_to_advance;
                        self.offset += if self.partially_consumed_tab { 0 } else { 1 };
                        count -= chars_to_advance;
                    } else {
                        self.partially_consumed_tab = false;
                        self.column += chars_to_tab;
                        self.offset += 1;
                        count -= 1;
                    }
                }
                _ => {
                    self.partially_consumed_tab = false;
                    self.offset += 1;
                    self.column += 1;
                    count -= 1;
                }
            }
        }
    }

    fn parse_block_quote_prefix(&mut self, line: &mut String) -> bool {
        let indent = self.indent;
        if indent <= 3 && line.as_bytes()[self.first_nonspace] == '>' as u8 {
            self.advance_offset(line, indent + 1, true);

            if strings::is_space_or_tab(&line.as_bytes()[self.offset]) {
                self.advance_offset(line, 1, true);
            }

            return true;
        }

        false
    }

    fn parse_node_item_prefix(&mut self,
                              line: &mut String,
                              container: &'a AstNode<'a>,
                              nl: &NodeList)
                              -> bool {
        if self.indent >= nl.marker_offset + nl.padding {
            self.advance_offset(line, nl.marker_offset + nl.padding, true);
            true
        } else if self.blank && container.first_child().is_some() {
            let offset = self.first_nonspace - self.offset;
            self.advance_offset(line, offset, false);
            true
        } else {
            false
        }
    }

    fn parse_code_block_prefix(&mut self,
                               line: &mut String,
                               container: &'a AstNode<'a>,
                               ast: &mut Ast,
                               should_continue: &mut bool)
                               -> bool {
        let ncb = match ast.value {
                NodeValue::CodeBlock(ref ncb) => Some(ncb.clone()),
                _ => None,
            }
            .unwrap();

        if !ncb.fenced {
            if self.indent >= CODE_INDENT {
                self.advance_offset(line, CODE_INDENT, true);
                return true;
            } else if self.blank {
                let offset = self.first_nonspace - self.offset;
                self.advance_offset(line, offset, false);
                return true;
            }
            return false;
        }

        let matched = if self.indent <= 3 &&
                         line.as_bytes()[self.first_nonspace] == ncb.fence_char {
            scanners::close_code_fence(&line[self.first_nonspace..]).unwrap_or(0)
        } else {
            0
        };

        if matched >= ncb.fence_length {
            *should_continue = false;
            self.advance_offset(line, matched, false);
            self.current = self.finalize_borrowed(container, ast).unwrap();
            return false;

        }

        let mut i = ncb.fence_offset;
        while i > 0 && strings::is_space_or_tab(&line.as_bytes()[self.offset]) {
            self.advance_offset(line, 1, true);
            i -= 1;
        }
        true
    }

    fn parse_html_block_prefix(&mut self, t: u8) -> bool {
        match t {
            1 | 2 | 3 | 4 | 5 => true,
            6 | 7 => !self.blank,
            _ => {
                assert!(false);
                false
            }
        }
    }

    fn add_child(&mut self,
                 mut parent: &'a AstNode<'a>,
                 value: NodeValue,
                 start_column: usize)
                 -> &'a AstNode<'a> {
        while !nodes::can_contain_type(parent, &value) {
            parent = self.finalize(parent).unwrap();
        }

        let child = make_block(value, self.line_number, start_column);
        let node = self.arena.alloc(Node::new(RefCell::new(child)));
        parent.append(node);
        node
    }

    fn add_text_to_container(&mut self,
                             mut container: &'a AstNode<'a>,
                             last_matched_container: &'a AstNode<'a>,
                             line: &mut String) {
        self.find_first_nonspace(line);

        if self.blank {
            if let Some(last_child) = container.last_child() {
                last_child.data.borrow_mut().last_line_blank = true;
            }
        }

        container.data.borrow_mut().last_line_blank = self.blank &&
                                                      match &container.data.borrow().value {
            &NodeValue::BlockQuote |
            &NodeValue::Heading(..) |
            &NodeValue::ThematicBreak => false,
            &NodeValue::CodeBlock(ref ncb) => !ncb.fenced,
            &NodeValue::Item(..) => {
                container.first_child().is_some() ||
                container.data.borrow().start_line != self.line_number
            }
            _ => true,
        };

        let mut tmp = container;
        while let Some(parent) = tmp.parent() {
            parent.data.borrow_mut().last_line_blank = false;
            tmp = parent;
        }

        if !self.current.same_node(last_matched_container) &&
           container.same_node(last_matched_container) && !self.blank &&
           match &self.current.data.borrow().value {
            &NodeValue::Paragraph => true,
            _ => false,
        } {
            self.add_line(self.current, line);
        } else {
            while !self.current.same_node(last_matched_container) {
                self.current = self.finalize(self.current).unwrap();
            }

            // TODO: remove this awful clone
            let node_type = container.data.borrow().value.clone();
            match &node_type {
                &NodeValue::CodeBlock(..) => {
                    self.add_line(container, line);
                }
                &NodeValue::HtmlBlock(ref nhb) => {
                    self.add_line(container, line);

                    let matches_end_condition = match nhb.block_type {
                        1 => scanners::html_block_end_1(&line[self.first_nonspace..]).is_some(),
                        2 => scanners::html_block_end_2(&line[self.first_nonspace..]).is_some(),
                        3 => scanners::html_block_end_3(&line[self.first_nonspace..]).is_some(),
                        4 => scanners::html_block_end_4(&line[self.first_nonspace..]).is_some(),
                        5 => scanners::html_block_end_5(&line[self.first_nonspace..]).is_some(),
                        _ => false,
                    };

                    if matches_end_condition {
                        container = self.finalize(container).unwrap();
                    }
                }
                _ => {
                    if self.blank {
                        // do nothing
                    } else if container.data.borrow().value.accepts_lines() {
                        match &container.data.borrow().value {
                            &NodeValue::Heading(ref nh) => {
                                if !nh.setext {
                                    strings::chop_trailing_hashtags(line);
                                }
                            }
                            _ => (),
                        };
                        let count = self.first_nonspace - self.offset;
                        self.advance_offset(line, count, false);
                        self.add_line(container, line);
                    } else {
                        let start_column = self.first_nonspace + 1;
                        container = self.add_child(container, NodeValue::Paragraph, start_column);
                        let count = self.first_nonspace - self.offset;
                        self.advance_offset(line, count, false);
                        self.add_line(container, line);
                    }
                }
            }

            self.current = container;
        }
    }

    fn add_line(&mut self, node: &'a AstNode<'a>, line: &mut String) {
        let mut ast = node.data.borrow_mut();
        assert!(ast.open);
        if self.partially_consumed_tab {
            self.offset += 1;
            let chars_to_tab = TAB_STOP - (self.column % TAB_STOP);
            for _ in 0..chars_to_tab {
                ast.content.push(' ');
            }
        }
        if self.offset < line.len() {
            ast.content += &line[self.offset..];
        }
    }

    pub fn finish(&mut self) -> &'a AstNode<'a> {
        if self.linebuf.len() > 0 {
            let linebuf = mem::replace(&mut self.linebuf, String::new());
            self.process_line(&linebuf);
        }

        self.finalize_document();

        self.consolidate_text_nodes(self.root);

        self.root
    }

    fn finalize_document(&mut self) {
        while !self.current.same_node(self.root) {
            self.current = self.finalize(self.current).unwrap();
        }

        self.finalize(self.root);
        self.process_inlines();
    }

    fn finalize(&mut self, node: &'a AstNode<'a>) -> Option<&'a AstNode<'a>> {
        self.finalize_borrowed(node, &mut *node.data.borrow_mut())
    }

    fn finalize_borrowed(&mut self,
                         node: &'a AstNode<'a>,
                         ast: &mut Ast)
                         -> Option<&'a AstNode<'a>> {
        assert!(ast.open);
        ast.open = false;

        if self.linebuf.len() == 0 {
            ast.end_line = self.line_number;
            ast.end_column = self.last_line_length;
        } else if match &ast.value {
            &NodeValue::Document => true,
            &NodeValue::CodeBlock(ref ncb) => ncb.fenced,
            &NodeValue::Heading(ref nh) => nh.setext,
            _ => false,
        } {
            ast.end_line = self.line_number;
            ast.end_column = self.linebuf.len();
            if ast.end_column > 0 && self.linebuf.as_bytes()[ast.end_column - 1] == '\n' as u8 {
                ast.end_column -= 1;
            }
            if ast.end_column > 0 && self.linebuf.as_bytes()[ast.end_column - 1] == '\r' as u8 {
                ast.end_column -= 1;
            }
        } else {
            ast.end_line = self.line_number - 1;
            ast.end_column = self.last_line_length;
        }

        let content = &mut ast.content;
        let mut pos = 0;

        let parent = node.parent();

        match &mut ast.value {
            &mut NodeValue::Paragraph => {
                while content.len() > 0 && content.as_bytes()[0] == '[' as u8 &&
                      unwrap_into(self.parse_reference_inline(content), &mut pos) {
                    while pos > 0 {
                        pos -= content.remove(0).len_utf8();
                    }
                }
                if strings::is_blank(content) {
                    node.detach();
                }
            }
            &mut NodeValue::CodeBlock(ref mut ncb) => {
                if !ncb.fenced {
                    strings::remove_trailing_blank_lines(content);
                    content.push('\n');
                } else {
                    let mut pos = 0;
                    while pos < content.len() {
                        if strings::is_line_end_char(&content.as_bytes()[pos]) {
                            break;
                        }
                        pos += 1;
                    }
                    assert!(pos < content.len());

                    let mut tmp = entity::unescape_html(&content[..pos]);
                    strings::trim(&mut tmp);
                    strings::unescape(&mut tmp);
                    ncb.info = tmp;

                    if content.as_bytes()[pos] == '\r' as u8 {
                        pos += 1;
                    }
                    if content.as_bytes()[pos] == '\n' as u8 {
                        pos += 1;
                    }

                    while pos > 0 {
                        pos -= content.remove(0).len_utf8();
                    }
                }
                ncb.literal = content.clone();
                content.clear();
            }
            &mut NodeValue::HtmlBlock(ref mut nhb) => {
                nhb.literal = content.clone();
                content.clear();
            }
            &mut NodeValue::List(ref mut nl) => {
                nl.tight = true;
                let mut ch = node.first_child();

                while let Some(item) = ch {
                    if item.data.borrow().last_line_blank && item.next_sibling().is_some() {
                        nl.tight = false;
                        break;
                    }

                    let mut subch = item.first_child();
                    while let Some(subitem) = subch {
                        if nodes::ends_with_blank_line(subitem) &&
                           (item.next_sibling().is_some() || subitem.next_sibling().is_some()) {
                            nl.tight = false;
                            break;
                        }
                        subch = subitem.next_sibling();
                    }

                    if !nl.tight {
                        break;
                    }

                    ch = item.next_sibling();
                }
            }
            _ => (),
        }

        parent
    }

    fn process_inlines(&mut self) {
        self.process_inlines_node(self.root);
    }

    fn process_inlines_node(&mut self, node: &'a AstNode<'a>) {
        if node.data.borrow().value.contains_inlines() {
            self.parse_inlines(node);
        }

        for n in node.children() {
            self.process_inlines_node(n);
        }
    }

    fn parse_inlines(&mut self, node: &'a AstNode<'a>) {
        let mut subj = inlines::Subject::new(self.arena,
                                             self.options,
                                             &node.data.borrow().content,
                                             &mut self.refmap);

        strings::rtrim(&mut subj.input);

        while !subj.eof() && subj.parse_inline(node) {}

        subj.process_emphasis(-1);

        while subj.pop_bracket() {}
    }

    fn consolidate_text_nodes(&mut self, node: &'a AstNode<'a>) {
        let mut nch = node.first_child();

        while let Some(n) = nch {
            let mut this_bracket = false;
            loop {
                match &mut n.data.borrow_mut().value {
                    &mut NodeValue::Text(ref mut root) => {
                        let ns = match n.next_sibling() {
                            Some(ns) => ns,
                            _ => {
                                if self.options.ext_autolink {
                                    autolink::process_autolinks(self.arena, n, root);
                                }
                                break;
                            }
                        };

                        match &ns.data.borrow().value {
                            &NodeValue::Text(ref adj) => {
                                *root += adj;
                                ns.detach();
                            }
                            _ => break,
                        }
                    }
                    &mut NodeValue::Link(..) |
                    &mut NodeValue::Image(..) => {
                        this_bracket = true;
                        break;
                    }
                    _ => break,
                }
            }

            if !this_bracket {
                self.consolidate_text_nodes(n);
            }

            nch = n.next_sibling();
        }
    }

    fn parse_reference_inline(&mut self, content: &str) -> Option<usize> {
        let mut subj = inlines::Subject::new(self.arena, self.options, content, &mut self.refmap);

        let mut lab = match subj.link_label() {
                Some(lab) => if lab.len() == 0 { return None } else { lab },
                None => return None,
            }
            .to_string();

        if subj.peek_char() != Some(&(':' as u8)) {
            return None;
        }

        subj.pos += 1;
        subj.spnl();
        let matchlen = match inlines::manual_scan_link_url(&subj.input[subj.pos..]) {
            Some(matchlen) => matchlen,
            None => return None,
        };
        let url = subj.input[subj.pos..subj.pos + matchlen].to_string();
        subj.pos += matchlen;

        let beforetitle = subj.pos;
        subj.spnl();
        let title = match scanners::link_title(&subj.input[subj.pos..]) {
            Some(matchlen) => {
                let t = &subj.input[subj.pos..subj.pos + matchlen];
                subj.pos += matchlen;
                t.to_string()
            }
            _ => {
                subj.pos = beforetitle;
                String::new()
            }
        };

        subj.skip_spaces();
        if !subj.skip_line_end() {
            if title.len() > 0 {
                subj.pos = beforetitle;
                subj.skip_spaces();
                if !subj.skip_line_end() {
                    return None;
                }
            } else {
                return None;
            }
        }

        lab = strings::normalize_reference_label(&lab);
        if lab.len() > 0 {
            subj.refmap.entry(lab).or_insert(Reference {
                url: strings::clean_url(&url),
                title: strings::clean_title(&title),
            });
        }
        Some(subj.pos)
    }
}

fn parse_list_marker(line: &mut String,
                     mut pos: usize,
                     interrupts_paragraph: bool)
                     -> Option<(usize, NodeList)> {
    let mut c = line.as_bytes()[pos];
    let startpos = pos;

    if c == '*' as u8 || c == '-' as u8 || c == '+' as u8 {
        pos += 1;
        if !isspace(&line.as_bytes()[pos]) {
            return None;
        }

        if interrupts_paragraph {
            let mut i = pos;
            while strings::is_space_or_tab(&line.as_bytes()[i]) {
                i += 1;
            }
            if line.as_bytes()[i] == '\n' as u8 {
                return None;
            }
        }

        return Some((pos - startpos,
                     NodeList {
                         list_type: ListType::Bullet,
                         marker_offset: 0,
                         padding: 0,
                         start: 1,
                         delimiter: ListDelimType::Period,
                         bullet_char: c,
                         tight: false,
                     }));
    } else if isdigit(&c) {
        let mut start: usize = 0;
        let mut digits = 0;

        loop {
            start = (10 * start) + (line.as_bytes()[pos] - '0' as u8) as usize;
            pos += 1;
            digits += 1;

            if !(digits < 9 && isdigit(&line.as_bytes()[pos])) {
                break;
            }
        }

        if interrupts_paragraph && start != 1 {
            return None;
        }

        c = line.as_bytes()[pos];
        if c != '.' as u8 && c != ')' as u8 {
            return None;
        }

        pos += 1;

        if !isspace(&line.as_bytes()[pos]) {
            return None;
        }

        if interrupts_paragraph {
            let mut i = pos;
            while strings::is_space_or_tab(&line.as_bytes()[i]) {
                i += 1;
            }
            if strings::is_line_end_char(&line.as_bytes()[i]) {
                return None;
            }
        }

        return Some((pos - startpos,
                     NodeList {
                         list_type: ListType::Ordered,
                         marker_offset: 0,
                         padding: 0,
                         start: start,
                         delimiter: if c == '.' as u8 {
                             ListDelimType::Period
                         } else {
                             ListDelimType::Paren
                         },
                         bullet_char: 0,
                         tight: false,
                     }));
    }

    None
}

pub fn unwrap_into<T>(t: Option<T>, out: &mut T) -> bool {
    match t {
        Some(v) => {
            *out = v;
            true
        }
        _ => false,
    }
}

pub fn unwrap_into_copy<T: Copy>(t: Option<&T>, out: &mut T) -> bool {
    match t {
        Some(v) => {
            *out = *v;
            true
        }
        _ => false,
    }
}

fn unwrap_into_2<T, U>(tu: Option<(T, U)>, out_t: &mut T, out_u: &mut U) -> bool {
    match tu {
        Some((t, u)) => {
            *out_t = t;
            *out_u = u;
            true
        }
        _ => false,
    }
}

fn lists_match(list_data: &NodeList, item_data: &NodeList) -> bool {
    list_data.list_type == item_data.list_type && list_data.delimiter == item_data.delimiter &&
    list_data.bullet_char == item_data.bullet_char
}