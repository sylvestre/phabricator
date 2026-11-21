//! The Harbormaster queries as `PhabricatorCursorPagedPolicyAwareQuery`
//! runs them: a `WHERE` built from the constraints, the builtin order, and
//! the policy-aware paging of [`pager::execute`].

use std::fmt::Write;
use std::marker::PhantomData;

use phab_conduit::order::{self, Column};
use phab_conduit::pager::{self, Direction, Paged, Paging, PolicyAwareQuery, core_error};
use phab_conduit::search::Clauses;
use phab_conduit::{ApiRequest, MethodError};
use phab_db::rows;
use serde_json::{Value, json};

use crate::storage::{self, Object};

/// The orderable `name` of build plans (`'reverse' => true`).
const PLAN_NAME: Column = Column::new("plan.name").ascending();

/// A builtin order (`getBuiltinOrders()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// `newest` (alias `created`), the default: by decreasing ID.
    Newest,
    /// `oldest`: by increasing ID.
    Oldest,
    /// `name`, which only build plans have: by name, then ID.
    Name,
}

impl Order {
    /// `setOrder()` with the `order` parameter; order vectors are left to
    /// PHP. `with_name` for `HarbormasterBuildPlanQuery`, whose `name`
    /// comes first.
    pub(crate) fn parse(
        value: Option<&Value>,
        query_class: &str,
        with_name: bool,
    ) -> Result<Self, MethodError> {
        let key = match value {
            None | Some(Value::Null) => return Ok(Self::Newest),
            Some(Value::String(key)) => key.as_str(),
            Some(_) => return Err(MethodError::Unsupported("order vectors")),
        };
        match key {
            "newest" | "created" => Ok(Self::Newest),
            "oldest" => Ok(Self::Oldest),
            "name" if with_name => Ok(Self::Name),
            other => Err(core_error(format!(
                "Query \"{query_class}\" does not support a builtin order \"{other}\". \
                 Supported orders are: {}newest, created, oldest.",
                if with_name { "name, " } else { "" }
            ))),
        }
    }

    fn columns(self, id: Column) -> Vec<Column> {
        match self {
            Self::Newest => vec![id],
            Self::Oldest => vec![id.reversed()],
            Self::Name => vec![PLAN_NAME, id],
        }
    }
}

/// A query on the rows of `T`.
pub(crate) struct Query<'a, T> {
    pub(crate) request: &'a ApiRequest<'a>,
    /// The constraints' conditions.
    pub(crate) clauses: Clauses,
    pub(crate) order: Order,
    pub(crate) marker: PhantomData<fn() -> T>,
}

impl<'a, T: Object> Query<'a, T> {
    pub(crate) fn new(request: &'a ApiRequest<'a>, clauses: Clauses, order: Order) -> Self {
        Self {
            request,
            clauses,
            order,
            marker: PhantomData,
        }
    }

    /// `newPagingMapFromPartialObject()`: the values of the order's
    /// columns for the row with ID `id`.
    async fn paging_values(&self, id: u64) -> Result<Vec<Value>, MethodError> {
        if self.order != Order::Name {
            return Ok(vec![json!(id)]);
        }
        let database = &self.request.context.database;
        let sql = format!(
            "SELECT name FROM {} WHERE id = ?",
            database.table("harbormaster", T::TABLE)
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(id)
            .fetch_optional(database.pool())
            .await?
            .ok_or_else(|| MethodError::internal("the cursor's row vanished"))?;
        Ok(vec![json!(rows::text(&row, "name")?), json!(id)])
    }

    /// `executeWithCursorPager()`: the page `paging` asks for, with the
    /// cursors around it.
    pub(crate) async fn page(&self, paging: &Paging) -> Result<Paged<T>, MethodError> {
        let (start, direction) = paging.start();
        let found = pager::execute(self, start, direction, paging.size.value + 1).await?;
        Ok(paging.pager().slice(found))
    }

    /// `execute()` without a limit.
    pub(crate) async fn all(&self) -> Result<Vec<T>, MethodError> {
        let rows = self.load(None, None, Direction::Forward, None).await?;
        T::visible(self.request, rows).await
    }
}

impl<T: Object> PolicyAwareQuery for Query<'_, T> {
    type Row = T;

    fn query_class(&self) -> &'static str {
        T::QUERY_CLASS
    }

    fn id(row: &T) -> u64 {
        u64::try_from(row.id()).unwrap_or_default()
    }

    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = T::CURSOR_RAISES_POLICY_EXCEPTIONS;

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<T>, MethodError> {
        // A cursor's own query has none of the constraints.
        let mut clauses = match ids {
            Some(ids) => {
                let mut clauses = Clauses::default();
                clauses.in_list(T::ID.sql, ids.iter().map(|id| json!(id)));
                clauses
            }
            None => self.clauses.clone(),
        };
        let columns = self.order.columns(T::ID);
        let reversed = direction == Direction::Backward;
        if let Some(after) = after {
            let values = self.paging_values(after).await?;
            if let Some((sql, binds)) = order::paging_clause(&columns, &values, reversed) {
                clauses.condition(sql, binds);
            }
        }
        let mut tail = order::order_by(&columns, reversed);
        if let Some(limit) = limit {
            let _ = write!(tail, " LIMIT {limit}");
        }
        storage::select(&self.request.context.database, &clauses, &tail).await
    }

    async fn filter(&self, rows: Vec<T>) -> Result<Vec<T>, MethodError> {
        T::visible(self.request, rows).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders() {
        assert_eq!(Order::parse(None, "Q", false), Ok(Order::Newest));
        assert_eq!(
            Order::parse(Some(&json!("created")), "Q", false),
            Ok(Order::Newest)
        );
        assert_eq!(
            Order::parse(Some(&json!("name")), "Q", true),
            Ok(Order::Name)
        );
        assert_eq!(
            Order::parse(Some(&json!("name")), "Q", false),
            Err(core_error(
                "Query \"Q\" does not support a builtin order \"name\". Supported orders are: \
                 newest, created, oldest."
                    .to_owned()
            ))
        );
        assert_eq!(
            Order::parse(Some(&json!("x")), "P", true),
            Err(core_error(
                "Query \"P\" does not support a builtin order \"x\". Supported orders are: \
                 name, newest, created, oldest."
                    .to_owned()
            ))
        );
        assert_eq!(
            order::order_by(&Order::Name.columns(Column::new("plan.id")), false),
            " ORDER BY plan.name ASC, plan.id DESC"
        );
    }
}
