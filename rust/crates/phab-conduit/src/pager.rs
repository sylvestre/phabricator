//! `PhabricatorCursorPagedPolicyAwareQuery::executeWithCursorPager()`: the
//! `limit`, `after` and `before` parameters, the policy-aware loading loop
//! of `PhabricatorPolicyAwareQuery::execute()`, and the page cut out of the
//! results with the cursors around it.

use serde_json::Value;

use crate::MethodError;
use phab_core::php;

/// The largest page `PhabricatorPolicyAwareQuery::execute()` loads.
const MAX_RAW_ROWS: usize = 1024;

/// The page size, as `AphrontCursorPagerView::setPageSize()` stores it.
#[derive(Debug, Clone, PartialEq)]
pub struct PageSize {
    /// What `cursor.limit` shows: `max(1, $limit)` keeps a string a string.
    pub shown: Value,
    /// The size.
    pub value: usize,
}

impl PageSize {
    /// The `limit` parameter; falsy means `default`.
    pub fn parse(limit: Option<&Value>, default: usize) -> Result<Self, MethodError> {
        let Some(limit) = limit.filter(|limit| php::is_truthy(limit)) else {
            return Ok(Self {
                shown: default.into(),
                value: default,
            });
        };
        let value: i64 = match limit {
            Value::Number(number) => number.as_i64(),
            Value::String(text) if is_integer(text) => text.parse().ok(),
            _ => None,
        }
        .ok_or(MethodError::Unsupported("an unusual limit"))?;
        // max() returns its first argument when both are equal.
        if value <= 1 {
            return Ok(Self {
                shown: 1.into(),
                value: 1,
            });
        }
        Ok(Self {
            shown: limit.clone(),
            value: usize::try_from(value).map_err(|_| MethodError::Unsupported("a huge limit"))?,
        })
    }
}

impl PageSize {
    /// The `limit` of a search method
    /// (`PhabricatorApplicationSearchEngine::setPagerLimitForConduit()`):
    /// 100 by default, between 1 and 100.
    pub fn for_search(limit: Option<&Value>) -> Result<Self, MethodError> {
        let Some(limit) = limit.filter(|limit| !limit.is_null()) else {
            return Ok(Self {
                shown: 100.into(),
                value: 100,
            });
        };
        let (value, text) = match limit {
            Value::Number(number) if !number.is_f64() => (number.as_i64(), number.to_string()),
            Value::String(text) if is_integer(text) => (text.parse().ok(), text.clone()),
            _ => (None, String::new()),
        };
        let value = value.ok_or(MethodError::Unsupported("an unusual limit"))?;
        if value > 100 {
            return Err(core_error(format!(
                "Maximum page size for Conduit API method calls is 100, but this call \
                 specified {text}."
            )));
        }
        if value < 1 {
            return Err(core_error(format!(
                "Minimum page size for API searches is 1, but this call specified {text}."
            )));
        }
        Ok(Self {
            shown: limit.clone(),
            value: usize::try_from(value).map_err(|_| MethodError::Unsupported("a huge limit"))?,
        })
    }
}

/// An optional `-` followed by digits, which PHP compares as an integer.
fn is_integer(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// An `after` or `before` value given to the pager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// `phutil_string_cast()` of the value.
    pub text: String,
    /// PHP truthiness of the value, which the pager tests.
    pub truthy: bool,
}

impl Cursor {
    /// `phutil_nonempty_scalar()` then `phutil_string_cast()`.
    pub fn parse(value: Option<&Value>) -> Result<Option<Self>, MethodError> {
        let Some(value) = value else {
            return Ok(None);
        };
        let text = match value {
            Value::Null | Value::Bool(false) => return Ok(None),
            Value::String(text) if text.is_empty() => return Ok(None),
            Value::String(text) => text.clone(),
            Value::Bool(true) => "1".to_owned(),
            Value::Number(number) if !number.is_f64() => number.to_string(),
            _ => return Err(MethodError::Unsupported("an unusual cursor")),
        };
        Ok(Some(Self {
            text,
            truthy: php::is_truthy(value),
        }))
    }

    /// The ID the cursor names; an `IN (%Ld)` casts it to an integer.
    pub fn id(&self) -> Result<u64, MethodError> {
        if !self.text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(MethodError::Unsupported("a non-numeric cursor"));
        }
        self.text
            .parse()
            .map_err(|_| MethodError::Unsupported("a huge cursor"))
    }
}

/// Which way a query pages through its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// In the query's order, after the cursor.
    Forward,
    /// Against it, before the cursor; the results are reversed back.
    Backward,
}

/// The `limit`, `after` and `before` parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct Paging {
    /// The page size.
    pub size: PageSize,
    /// `after`.
    pub after: Option<Cursor>,
    /// `before`.
    pub before: Option<Cursor>,
}

impl Paging {
    /// Reads the parameters of a search method's request.
    pub fn for_search(
        limit: Option<&Value>,
        after: Option<&Value>,
        before: Option<&Value>,
    ) -> Result<Self, MethodError> {
        Ok(Self {
            size: PageSize::for_search(limit)?,
            after: Cursor::parse(after)?,
            before: Cursor::parse(before)?,
        })
    }

    /// Reads the parameters of a request.
    pub fn parse(
        limit: Option<&Value>,
        after: Option<&Value>,
        before: Option<&Value>,
        default_limit: usize,
    ) -> Result<Self, MethodError> {
        Ok(Self {
            size: PageSize::parse(limit, default_limit)?,
            after: Cursor::parse(after)?,
            before: Cursor::parse(before)?,
        })
    }

    /// The cursor the query starts from, and the way it goes: `after`
    /// wins over `before`.
    pub fn start(&self) -> (Option<&Cursor>, Direction) {
        match (&self.after, &self.before) {
            (Some(after), _) => (Some(after), Direction::Forward),
            (None, Some(before)) => (Some(before), Direction::Backward),
            (None, None) => (None, Direction::Forward),
        }
    }

    /// How the results are cut into a page.
    pub fn pager(&self) -> Pager {
        Pager {
            page_size: self.size.value,
            after: self.after.as_ref().is_some_and(|after| after.truthy),
            before: self.before.as_ref().is_some_and(|before| before.truthy),
        }
    }
}

/// A query whose rows have IDs it pages by, and which the viewer may or
/// may not see.
pub trait PolicyAwareQuery {
    /// A row of the query.
    type Row: Send;

    /// The PHP class of the query, for error messages.
    fn query_class(&self) -> &'static str;

    /// The row's ID, which cursors name.
    fn id(row: &Self::Row) -> u64;

    /// Whether a cursor naming a row the viewer can't see raises PHP's
    /// policy exception (the policy filter rejects it) rather than the
    /// "does not identify a valid object" one (the query drops it first).
    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = false;

    /// Up to `limit` rows after the row with ID `after`, going in
    /// `direction` (`ids` instead: the rows with these IDs).
    fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<Vec<Self::Row>, MethodError>> + Send;

    /// The rows the viewer can see, in their order.
    fn filter(
        &self,
        rows: Vec<Self::Row>,
    ) -> impl Future<Output = Result<Vec<Self::Row>, MethodError>> + Send;

    /// [`Self::filter()`] for the row a cursor names, which PHP loads with
    /// a query of its own: a query whose filter keeps state between pages
    /// must not keep it from this one.
    fn filter_cursor(
        &self,
        rows: Vec<Self::Row>,
    ) -> impl Future<Output = Result<Vec<Self::Row>, MethodError>> + Send {
        self.filter(rows)
    }
}

/// `ERR-CONDUIT-CORE` with `message`, as an uncaught exception gives.
pub fn core_error(message: String) -> MethodError {
    MethodError::Conduit {
        code: "ERR-CONDUIT-CORE".to_owned(),
        info: Some(message),
    }
}

/// `PhutilNumber`'s default formatting.
pub fn thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `PhabricatorPolicyAwareQuery::execute()` with a limit of `need`: loads
/// pages of rows until `need` of them are visible, giving up ("overheated")
/// after ten times as many raw rows.
pub async fn execute<Q: PolicyAwareQuery + Sync>(
    query: &Q,
    start: Option<&Cursor>,
    direction: Direction,
    need: usize,
) -> Result<Vec<Q::Row>, MethodError> {
    // newInternalCursorFromExternalCursor(): the cursor must be a row the
    // viewer can see.
    let mut after = None;
    if let Some(cursor) = start {
        let id = cursor.id()?;
        let rows = query
            .load(Some(&[id]), None, Direction::Forward, None)
            .await?;
        let exists = !rows.is_empty();
        if query.filter_cursor(rows).await?.is_empty() {
            // PHP raises the policy exception, whose message explains the
            // object's policy.
            if exists && Q::CURSOR_RAISES_POLICY_EXCEPTIONS {
                return Err(MethodError::Unsupported("a cursor the viewer can't see"));
            }
            return Err(core_error(format!(
                "Cursor \"{}\" does not identify a valid object in query \"{}\".",
                cursor.text,
                query.query_class()
            )));
        }
        after = Some(id);
    }

    // PHP's integer overflows to a float there, which is no limit either.
    let overheat_limit = need.saturating_mul(10);
    let mut total_seen = 0;
    let mut results = Vec::new();
    'pages: loop {
        let limit = (need - results.len()).min(MAX_RAW_ROWS);
        let rows = query.load(None, after, direction, Some(limit)).await?;
        let seen = rows.len();
        total_seen += seen;
        // nextPage(): paging continues after the last raw row.
        if let Some(last) = rows.last() {
            after = Some(Q::id(last));
        }
        for row in query.filter(rows).await? {
            results.push(row);
            if results.len() >= need {
                break 'pages;
            }
        }
        if seen < limit {
            break;
        }
        if total_seen >= overheat_limit {
            return Err(core_error(format!(
                "Query (of class \"{}\") overheated: examined more than {} raw rows \
                 without finding {} visible objects.",
                query.query_class(),
                thousands(overheat_limit),
                thousands(need)
            )));
        }
    }
    if direction == Direction::Backward {
        results.reverse();
    }
    Ok(results)
}

/// A page of results with the cursors to the pages around it.
#[derive(Debug, PartialEq)]
pub struct Paged<T> {
    /// The page.
    pub items: Vec<T>,
    /// The item `after` names to get the next page.
    pub next: Option<T>,
    /// The item `before` names to get the previous page.
    pub previous: Option<T>,
}

/// The state of `AphrontCursorPagerView` once the query ran.
#[derive(Debug, Clone, Copy)]
pub struct Pager {
    page_size: usize,
    /// Whether `getAfterID()` is truthy.
    after: bool,
    /// Whether `getBeforeID()` is truthy.
    before: bool,
}

impl Pager {
    /// `sliceResults()` and the cursors `executeWithCursorPager()` sets.
    /// `results` holds up to `page_size + 1` items.
    pub fn slice<T: Clone>(self, mut results: Vec<T>) -> Paged<T> {
        let more = results.len() > self.page_size;
        if more {
            let offset = if self.before {
                results.len() - self.page_size
            } else {
                0
            };
            results.drain(..offset);
            results.truncate(self.page_size);
        }
        let next = results.last().filter(|_| self.before || more).cloned();
        let previous = results
            .first()
            .filter(|_| self.after || (self.before && more))
            .cloned();
        Paged {
            items: results,
            next,
            previous,
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    #[rstest]
    #[case(json!(null), json!(100), 100)]
    #[case(json!(0), json!(100), 100)]
    #[case(json!("0"), json!(100), 100)]
    #[case(json!(""), json!(100), 100)]
    #[case(json!(5), json!(5), 5)]
    #[case(json!("5"), json!("5"), 5)]
    #[case(json!("1"), json!(1), 1)]
    #[case(json!(-3), json!(1), 1)]
    #[case(json!("-3"), json!(1), 1)]
    fn limits_like_php(#[case] limit: Value, #[case] shown: Value, #[case] value: usize) {
        assert_eq!(
            PageSize::parse(Some(&limit), 100),
            Ok(PageSize { shown, value })
        );
    }

    #[rstest]
    #[case(json!("abc"))]
    #[case(json!("1e3"))]
    #[case(json!(2.5))]
    #[case(json!(true))]
    #[case(json!([1]))]
    fn leaves_unusual_limits_to_php(#[case] limit: Value) {
        assert!(matches!(
            PageSize::parse(Some(&limit), 100),
            Err(MethodError::Unsupported(_))
        ));
    }

    #[test]
    fn search_limits_like_php() {
        let limit = |value: Value| PageSize::for_search(Some(&value));
        assert_eq!(
            PageSize::for_search(None),
            Ok(PageSize {
                shown: json!(100),
                value: 100
            })
        );
        assert_eq!(
            limit(json!(null)),
            Ok(PageSize {
                shown: json!(100),
                value: 100
            })
        );
        assert_eq!(
            limit(json!("7")),
            Ok(PageSize {
                shown: json!("7"),
                value: 7
            })
        );
        assert_eq!(
            limit(json!(101)),
            Err(core_error(
                "Maximum page size for Conduit API method calls is 100, but this call specified \
                 101."
                    .to_owned()
            ))
        );
        assert_eq!(
            limit(json!("0")),
            Err(core_error(
                "Minimum page size for API searches is 1, but this call specified 0.".to_owned()
            ))
        );
        assert!(matches!(
            limit(json!(1.5)),
            Err(MethodError::Unsupported(_))
        ));
    }

    #[test]
    fn reads_cursors_like_php() {
        let cursor = |value: Value| Cursor::parse(Some(&value));
        assert_eq!(Cursor::parse(None), Ok(None));
        assert_eq!(cursor(json!("")), Ok(None));
        assert_eq!(cursor(json!(false)), Ok(None));
        assert_eq!(
            cursor(json!(0)),
            Ok(Some(Cursor {
                text: "0".to_owned(),
                truthy: false
            }))
        );
        assert_eq!(
            cursor(json!("12")),
            Ok(Some(Cursor {
                text: "12".to_owned(),
                truthy: true
            }))
        );
        assert!(cursor(json!([1])).is_err());
        assert!(cursor(json!(1.5)).is_err());
    }

    #[rstest]
    // A first page with more to come.
    #[case(vec![1, 2, 3], 2, false, false, vec![1, 2], Some(2), None)]
    // The last page after a cursor.
    #[case(vec![7], 2, true, false, vec![7], None, Some(7))]
    // Paging backwards keeps the items closest to the cursor.
    #[case(vec![1, 5], 1, false, true, vec![5], Some(5), Some(5))]
    #[case(vec![1], 100, false, true, vec![1], Some(1), None)]
    #[case(vec![], 2, true, true, vec![], None, None)]
    fn slices_like_the_pager(
        #[case] results: Vec<u32>,
        #[case] page: usize,
        #[case] after: bool,
        #[case] before: bool,
        #[case] items: Vec<u32>,
        #[case] next: Option<u32>,
        #[case] previous: Option<u32>,
    ) {
        assert_eq!(
            Pager {
                page_size: page,
                after,
                before
            }
            .slice(results),
            Paged {
                items,
                next,
                previous
            }
        );
    }

    #[test]
    fn formats_numbers_like_phutil_number() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(20), "20");
        assert_eq!(thousands(1010), "1,010");
        assert_eq!(thousands(123_456_789), "123,456,789");
    }
}
