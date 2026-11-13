//! Tables: the `| a | b |` syntax (`PhutilRemarkupSimpleTableBlockRule`)
//! and the shared renderer of `PhutilRemarkupBlockRule`.

use crate::engine::Context;
use crate::markup::{self, Content, Markup};
use crate::php;
use crate::preg::{self, Pattern};
use crate::rule::BlockRule;

static TABLE_LINE: Pattern = Pattern::new(r"^(\s*\|.*+\n?)+$");
static CELL: Pattern = Pattern::new(r"\|((?:(?:\[\[.*?\]\])|(?:[^|\[]+)|(?:\[[^\|\[]))*)");
static DASHES: Pattern = Pattern::new(r"^--+\z");

/// A cell type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellType {
    /// `<td>`.
    Data,
    /// `<th>`.
    Header,
}

impl CellType {
    fn tag(self) -> &'static str {
        match self {
            Self::Data => "td",
            Self::Header => "th",
        }
    }
}

/// A cell of a table to render.
#[derive(Debug, Clone)]
pub struct Cell {
    /// `td` or `th`.
    pub kind: CellType,
    /// The rendered content.
    pub content: Markup,
}

/// `renderRemarkupTable()`.
pub fn render_table(cx: &Context<'_>, rows: &[Vec<Cell>]) -> Markup {
    if cx.is_text_mode() {
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0; columns];
        let lengths: Vec<Vec<usize>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(column, cell)| {
                        let length = php::utf8_strlen(cx.restore_text(&cell.content).as_bytes());
                        widths[column] = widths[column].max(length);
                        length
                    })
                    .collect()
            })
            .collect();
        let mut out = Vec::new();
        for (row, row_lengths) in rows.iter().zip(&lengths) {
            let mut headings = false;
            for (column, cell) in row.iter().enumerate() {
                out.extend_from_slice(b"| ");
                out.extend_from_slice(cell.content.as_bytes());
                out.extend(std::iter::repeat_n(
                    b' ',
                    widths[column] - row_lengths[column],
                ));
                out.push(b' ');
                headings |= cell.kind == CellType::Header;
            }
            out.extend_from_slice(b"|\n");
            if headings {
                for (column, cell) in row.iter().enumerate() {
                    let fill = if cell.kind == CellType::Header {
                        b'-'
                    } else {
                        b' '
                    };
                    out.extend_from_slice(b"| ");
                    out.extend(std::iter::repeat_n(fill, widths[column]));
                    out.push(b' ');
                }
                out.extend_from_slice(b"|\n");
            }
        }
        return Markup::Text(php::rtrim(&out, b"\n").to_vec());
    }

    let mail = cx.is_html_mail_mode();
    let table_style: &[u8] = b"border-collapse: separate;\n          border-spacing: 1px;\n          background: #d3d3d3;\n          margin: 12px 0;";
    let cell_style: &[u8] = b"background: #ffffff;\n          padding: 3px 6px;";
    let cell_attributes: &[(&str, Option<&[u8]>)] = if mail {
        &[("style", Some(cell_style))]
    } else {
        &[]
    };

    let newline = Markup::text("\n");
    let mut out = vec![Some(newline.clone())];
    for row in rows {
        let cells: Vec<Option<Markup>> = row
            .iter()
            .map(|cell| {
                Some(markup::tag(
                    cell.kind.tag(),
                    cell_attributes,
                    Content::One(&cell.content),
                ))
            })
            .collect();
        out.push(Some(markup::tag("tr", &[], Content::List(&cells))));
        out.push(Some(newline.clone()));
    }
    let table_attributes: &[(&str, Option<&[u8]>)] = if mail {
        &[("style", Some(table_style))]
    } else {
        &[("class", Some(b"remarkup-table"))]
    };
    let table = markup::tag("table", table_attributes, Content::List(&out));
    markup::tag(
        "div",
        &[("class", Some(b"remarkup-table-wrap"))],
        Content::One(&table),
    )
}

/// `PhutilRemarkupSimpleTableBlockRule`.
pub struct SimpleTable;

impl BlockRule for SimpleTable {
    fn class_name(&self) -> &'static str {
        "PhutilRemarkupSimpleTableBlockRule"
    }

    fn matching_line_count(&self, lines: &[&[u8]], cursor: usize) -> usize {
        lines[cursor..]
            .iter()
            .take_while(|line| preg::is_match(&TABLE_LINE, line))
            .count()
    }

    fn markup_text(
        &self,
        cx: &mut Context<'_>,
        text: &[u8],
        _children: Option<Markup>,
    ) -> Option<Markup> {
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        for line in php::explode_lines(text) {
            // Ending delimiters don't make cells.
            let line = php::rtrim(line, b"|");
            let mut any_header = false;
            let mut any_content = false;
            let mut cells = Vec::new();
            for groups in preg::match_all(&CELL, line) {
                let cell = php::trim(groups.get(1));
                if !cell.is_empty() {
                    if preg::is_match(&DASHES, cell) {
                        any_header = true;
                    } else {
                        any_content = true;
                    }
                }
                cells.push(Cell {
                    kind: CellType::Data,
                    content: cx.apply_rules(Markup::text(cell)),
                });
            }

            // A row of "--" cells marks the row above as headers.
            if !any_header || any_content {
                rows.push(cells);
            } else if let Some(last) = rows.last_mut() {
                for (index, cell) in cells.iter().enumerate() {
                    if is_php_truthy(&cell.content)
                        && let Some(above) = last.get_mut(index)
                    {
                        above.kind = CellType::Header;
                    }
                }
            }
        }
        if rows.is_empty() {
            return Some(cx.apply_rules(Markup::text(text)));
        }
        Some(render_table(cx, &rows))
    }
}

/// `if ($value)` of a string or safe HTML value.
fn is_php_truthy(value: &Markup) -> bool {
    match value {
        Markup::Text(bytes) => !bytes.is_empty() && bytes != b"0",
        Markup::Html(_) => true,
    }
}
