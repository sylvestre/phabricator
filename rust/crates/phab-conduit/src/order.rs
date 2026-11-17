//! Ordering and paging by columns, like
//! `PhabricatorCursorPagedPolicyAwareQuery::formatOrderClause()` and
//! `buildPagingClauseFromMultipleColumns()`: an order is a list of
//! columns, the last of which is unique; the page after a row holds the
//! rows which come after its values in that order.

use serde_json::Value;

/// Where the rows whose value is `NULL` go (`'null'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nulls {
    /// First, in the forward direction.
    Head,
    /// Last, in the forward direction.
    Tail,
}

/// An orderable column (`getOrderableColumns()`), with the direction of
/// its component in the order vector folded into `reverse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    /// The column, with its table alias, like `s.epoch`.
    pub sql: &'static str,
    /// `'reverse'`: forward goes by increasing values. Columns go by
    /// decreasing values otherwise.
    pub reverse: bool,
    /// `'null'`: how `NULL` values sort.
    pub nulls: Option<Nulls>,
}

impl Column {
    /// A column going by decreasing values, without `NULL`s.
    pub const fn new(sql: &'static str) -> Self {
        Self {
            sql,
            reverse: false,
            nulls: None,
        }
    }

    /// Going by increasing values (`'reverse' => true`).
    #[must_use]
    pub const fn ascending(mut self) -> Self {
        self.reverse = true;
        self
    }

    /// With `NULL` values sorting as `nulls`.
    #[must_use]
    pub const fn nulls(mut self, nulls: Nulls) -> Self {
        self.nulls = Some(nulls);
        self
    }

    /// The column of a `-key` component of an order vector.
    #[must_use]
    pub const fn reversed(mut self) -> Self {
        self.reverse = !self.reverse;
        self
    }
}

/// `formatOrderClause()`: ` ORDER BY ...`; `query_reversed` when paging
/// backwards.
pub fn order_by(columns: &[Column], query_reversed: bool) -> String {
    let mut parts = Vec::new();
    for column in columns {
        let descending = !query_reversed ^ column.reverse;
        let direction = if descending { "DESC" } else { "ASC" };
        match column.nulls {
            Some(Nulls::Head) => parts.push(format!("({} IS NULL) {direction}", column.sql)),
            Some(Nulls::Tail) => parts.push(format!("({} IS NOT NULL) {direction}", column.sql)),
            None => {}
        }
        parts.push(format!("{} {direction}", column.sql));
    }
    format!(" ORDER BY {}", parts.join(", "))
}

/// `buildPagingClauseFromMultipleColumns()`: the condition selecting the
/// rows after the one whose values are `values` (one per column, `Null`
/// for SQL `NULL`), and the values it binds. `None` when nothing pages.
pub fn paging_clause(
    columns: &[Column],
    values: &[Value],
    query_reversed: bool,
) -> Option<(String, Vec<Value>)> {
    let mut clauses: Vec<(String, Vec<Value>)> = Vec::new();
    let mut accumulated: Vec<(String, Vec<Value>)> = Vec::new();
    for (column, value) in columns.iter().zip(values) {
        let reverse = query_reversed ^ column.reverse;
        let mut parts: Vec<(String, Vec<Value>)> = Vec::new();
        if let Some(nulls) = column.nulls {
            let (mut if_null, mut if_nonnull) = (nulls == Nulls::Head, nulls == Nulls::Tail);
            if reverse {
                if_null = !if_null;
                if_nonnull = !if_nonnull;
            }
            if if_null && value.is_null() {
                parts.push((format!("({} IS NOT NULL)", column.sql), Vec::new()));
            } else if if_nonnull && !value.is_null() {
                parts.push((format!("({} IS NULL)", column.sql), Vec::new()));
            }
        }
        if !value.is_null() {
            let comparison = if reverse { ">" } else { "<" };
            parts.push((
                format!("{} {comparison} ?", column.sql),
                vec![value.clone()],
            ));
        }
        let mut clause = accumulated.clone();
        if !parts.is_empty() {
            clause.push(join(parts, " OR "));
        }
        if !clause.is_empty() {
            clauses.push(join(clause, " AND "));
        }
        if value.is_null() {
            accumulated.push((format!("{} IS NULL", column.sql), Vec::new()));
        } else {
            accumulated.push((format!("{} = ?", column.sql), vec![value.clone()]));
        }
    }
    (!clauses.is_empty()).then(|| join(clauses, " OR "))
}

/// `%LO` and `%LA`: the parts in parentheses, joined.
fn join(parts: Vec<(String, Vec<Value>)>, glue: &str) -> (String, Vec<Value>) {
    let mut sql = Vec::with_capacity(parts.len());
    let mut binds = Vec::new();
    for (part, values) in parts {
        sql.push(format!("({part})"));
        binds.extend(values);
    }
    (sql.join(glue), binds)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ID: Column = Column::new("r.id");
    const EPOCH: Column = Column::new("s.epoch").nulls(Nulls::Tail);
    const NAME: Column = Column::new("r.name").ascending();

    #[test]
    fn order_clauses_like_php() {
        assert_eq!(order_by(&[ID], false), " ORDER BY r.id DESC");
        assert_eq!(order_by(&[ID.reversed()], false), " ORDER BY r.id ASC");
        assert_eq!(
            order_by(&[EPOCH, ID], false),
            " ORDER BY (s.epoch IS NOT NULL) DESC, s.epoch DESC, r.id DESC"
        );
        assert_eq!(
            order_by(&[NAME, ID], true),
            " ORDER BY r.name DESC, r.id ASC"
        );
    }

    #[test]
    fn paging_clauses_like_php() {
        assert_eq!(
            paging_clause(&[ID], &[json!(5)], false),
            Some(("(((r.id < ?)))".to_owned(), vec![json!(5)]))
        );
        assert_eq!(
            paging_clause(&[EPOCH, ID], &[json!(100), json!(5)], false),
            Some((
                "((((s.epoch IS NULL)) OR (s.epoch < ?))) OR ((s.epoch = ?) AND ((r.id < ?)))"
                    .to_owned(),
                vec![json!(100), json!(100), json!(5)]
            ))
        );
        assert_eq!(
            paging_clause(&[EPOCH, ID], &[Value::Null, json!(5)], false),
            Some((
                "((s.epoch IS NULL) AND ((r.id < ?)))".to_owned(),
                vec![json!(5)]
            ))
        );
        assert_eq!(
            paging_clause(&[EPOCH, ID], &[Value::Null, json!(5)], true),
            Some((
                "((((s.epoch IS NOT NULL)))) OR ((s.epoch IS NULL) AND ((r.id > ?)))".to_owned(),
                vec![json!(5)]
            ))
        );
    }
}
