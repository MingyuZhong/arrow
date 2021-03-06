// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! SQL Query Planner (produces logical plan from SQL AST)

use std::sync::Arc;

use crate::error::{ExecutionError, Result};
use crate::logicalplan::Expr::Alias;
use crate::logicalplan::{
    lit, Expr, FunctionMeta, LogicalPlan, LogicalPlanBuilder, Operator, ScalarValue,
};

use arrow::datatypes::*;

use sqlparser::ast::{
    BinaryOperator, DataType as SQLDataType, Expr as SQLExpr, Query, Select, SelectItem,
    SetExpr, TableFactor, TableWithJoins, UnaryOperator, Value,
};
use sqlparser::ast::{OrderByExpr, Statement};

/// The SchemaProvider trait allows the query planner to obtain meta-data about tables and
/// functions referenced in SQL statements
pub trait SchemaProvider {
    /// Getter for a field description
    fn get_table_meta(&self, name: &str) -> Option<SchemaRef>;
    /// Getter for a UDF description
    fn get_function_meta(&self, name: &str) -> Option<Arc<FunctionMeta>>;
}

/// SQL query planner
pub struct SqlToRel<S: SchemaProvider> {
    schema_provider: S,
}

impl<S: SchemaProvider> SqlToRel<S> {
    /// Create a new query planner
    pub fn new(schema_provider: S) -> Self {
        SqlToRel { schema_provider }
    }

    /// Generate a logic plan from an SQL statement
    pub fn statement_to_plan(&self, sql: &Statement) -> Result<LogicalPlan> {
        match sql {
            Statement::Query(query) => self.query_to_plan(&query),
            _ => Err(ExecutionError::NotImplemented(
                "Only SELECT statements are implemented".to_string(),
            )),
        }
    }

    /// Generate a logic plan from an SQL query
    pub fn query_to_plan(&self, query: &Query) -> Result<LogicalPlan> {
        let plan = match &query.body {
            SetExpr::Select(s) => self.select_to_plan(s.as_ref()),
            _ => Err(ExecutionError::NotImplemented(
                format!("Query {} not implemented yet", query.body).to_owned(),
            )),
        }?;

        let plan = self.order_by(&plan, &query.order_by)?;

        self.limit(&plan, &query.limit)
    }

    fn from_join_to_plan(&self, from: &Vec<TableWithJoins>) -> Result<LogicalPlan> {
        if from.len() == 0 {
            return Ok(LogicalPlanBuilder::empty().build()?);
        }
        if from.len() != 1 {
            return Err(ExecutionError::NotImplemented(
                "FROM with multiple tables is still not implemented".to_string(),
            ));
        };
        let relation = &from[0].relation;
        match relation {
            TableFactor::Table { name, .. } => {
                let name = name.to_string();
                match self.schema_provider.get_table_meta(&name) {
                    Some(schema) => Ok(LogicalPlanBuilder::scan(
                        "default",
                        &name,
                        schema.as_ref(),
                        None,
                    )?
                    .build()?),
                    None => Err(ExecutionError::General(format!(
                        "no schema found for table {}",
                        name
                    ))),
                }
            }
            _ => Err(ExecutionError::NotImplemented(
                "Subqueries are still not supported".to_string(),
            )),
        }
    }

    /// Generate a logic plan from an SQL select
    fn select_to_plan(&self, select: &Select) -> Result<LogicalPlan> {
        if select.having.is_some() {
            return Err(ExecutionError::NotImplemented(
                "HAVING is not implemented yet".to_string(),
            ));
        }

        let plan = self.from_join_to_plan(&select.from)?;

        // selection first
        let plan = self.filter(&plan, &select.selection)?;

        let projection_expr: Vec<Expr> = select
            .projection
            .iter()
            .map(|e| self.sql_select_to_rex(&e, &plan.schema()))
            .collect::<Result<Vec<Expr>>>()?;

        let aggr_expr: Vec<Expr> = projection_expr
            .iter()
            .filter(|e| is_aggregate_expr(e))
            .map(|e| e.clone())
            .collect();

        // apply projection or aggregate
        let plan = if (select.group_by.len() > 0) | (aggr_expr.len() > 0) {
            self.aggregate(&plan, projection_expr, &select.group_by, aggr_expr)?
        } else {
            self.project(&plan, projection_expr)?
        };
        Ok(plan)
    }

    /// Apply a filter to the plan
    fn filter(
        &self,
        plan: &LogicalPlan,
        selection: &Option<SQLExpr>,
    ) -> Result<LogicalPlan> {
        match *selection {
            Some(ref filter_expr) => LogicalPlanBuilder::from(&plan)
                .filter(self.sql_to_rex(filter_expr, &plan.schema())?)?
                .build(),
            _ => Ok(plan.clone()),
        }
    }

    /// Wrap a plan in a projection
    fn project(&self, input: &LogicalPlan, expr: Vec<Expr>) -> Result<LogicalPlan> {
        LogicalPlanBuilder::from(input).project(expr)?.build()
    }

    /// Wrap a plan in an aggregate
    fn aggregate(
        &self,
        input: &LogicalPlan,
        projection_expr: Vec<Expr>,
        group_by: &Vec<SQLExpr>,
        aggr_expr: Vec<Expr>,
    ) -> Result<LogicalPlan> {
        let group_expr: Vec<Expr> = group_by
            .iter()
            .map(|e| self.sql_to_rex(&e, &input.schema()))
            .collect::<Result<Vec<Expr>>>()?;

        let group_by_count = group_expr.len();
        let aggr_count = aggr_expr.len();

        if group_by_count + aggr_count != projection_expr.len() {
            return Err(ExecutionError::General(
                "Projection references non-aggregate values".to_owned(),
            ));
        }

        let plan = LogicalPlanBuilder::from(&input)
            .aggregate(group_expr, aggr_expr)?
            .build()?;

        // optionally wrap in projection to preserve final order of fields
        let expected_columns: Vec<String> = projection_expr
            .iter()
            .map(|e| e.name(input.schema()))
            .collect::<Result<Vec<_>>>()?;
        let columns: Vec<String> = plan
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect::<Vec<_>>();
        if expected_columns != columns {
            self.project(
                &plan,
                expected_columns
                    .iter()
                    .map(|c| Expr::Column(c.clone()))
                    .collect(),
            )
        } else {
            Ok(plan)
        }
    }

    /// Wrap a plan in a limit
    fn limit(&self, input: &LogicalPlan, limit: &Option<SQLExpr>) -> Result<LogicalPlan> {
        match *limit {
            Some(ref limit_expr) => {
                let n = match self.sql_to_rex(&limit_expr, &input.schema())? {
                    Expr::Literal(ScalarValue::Int64(n)) => Ok(n as usize),
                    _ => Err(ExecutionError::General(
                        "Unexpected expression for LIMIT clause".to_string(),
                    )),
                }?;

                LogicalPlanBuilder::from(&input).limit(n)?.build()
            }
            _ => Ok(input.clone()),
        }
    }

    /// Wrap the logical in a sort
    fn order_by(
        &self,
        plan: &LogicalPlan,
        order_by: &Vec<OrderByExpr>,
    ) -> Result<LogicalPlan> {
        if order_by.len() == 0 {
            return Ok(plan.clone());
        }

        let input_schema = plan.schema();
        let order_by_rex: Result<Vec<Expr>> = order_by
            .iter()
            .map(|e| {
                Ok(Expr::Sort {
                    expr: Box::new(self.sql_to_rex(&e.expr, &input_schema).unwrap()),
                    // by default asc
                    asc: e.asc.unwrap_or(true),
                    // by default nulls first to be consistent with spark
                    nulls_first: e.nulls_first.unwrap_or(true),
                })
            })
            .collect();

        LogicalPlanBuilder::from(&plan).sort(order_by_rex?)?.build()
    }

    /// Generate a relational expression from a select SQL expression
    fn sql_select_to_rex(&self, sql: &SelectItem, schema: &Schema) -> Result<Expr> {
        match sql {
            SelectItem::UnnamedExpr(expr) => self.sql_to_rex(expr, schema),
            SelectItem::ExprWithAlias { expr, alias } => Ok(Alias(
                Box::new(self.sql_to_rex(&expr, schema)?),
                alias.value.clone(),
            )),
            SelectItem::Wildcard => Ok(Expr::Wildcard),
            SelectItem::QualifiedWildcard(_) => Err(ExecutionError::NotImplemented(
                "Qualified wildcards are not supported".to_string(),
            )),
        }
    }

    /// Generate a relational expression from a SQL expression
    pub fn sql_to_rex(&self, sql: &SQLExpr, schema: &Schema) -> Result<Expr> {
        match sql {
            SQLExpr::Value(Value::Number(n)) => match n.parse::<i64>() {
                Ok(n) => Ok(lit(n)),
                Err(_) => Ok(lit(n.parse::<f64>().unwrap())),
            },
            SQLExpr::Value(Value::SingleQuotedString(ref s)) => Ok(lit(s.clone())),

            SQLExpr::Identifier(ref id) => match schema.field_with_name(&id.value) {
                Ok(field) => Ok(Expr::Column(field.name().clone())),
                Err(_) => Err(ExecutionError::ExecutionError(format!(
                    "Invalid identifier '{}' for schema {}",
                    id,
                    schema.to_string()
                ))),
            },

            SQLExpr::Wildcard => Ok(Expr::Wildcard),

            SQLExpr::Cast {
                ref expr,
                ref data_type,
            } => Ok(Expr::Cast {
                expr: Box::new(self.sql_to_rex(&expr, schema)?),
                data_type: convert_data_type(data_type)?,
            }),

            SQLExpr::IsNull(ref expr) => {
                Ok(Expr::IsNull(Box::new(self.sql_to_rex(expr, schema)?)))
            }

            SQLExpr::IsNotNull(ref expr) => {
                Ok(Expr::IsNotNull(Box::new(self.sql_to_rex(expr, schema)?)))
            }

            SQLExpr::UnaryOp { ref op, ref expr } => match *op {
                UnaryOperator::Not => {
                    Ok(Expr::Not(Box::new(self.sql_to_rex(expr, schema)?)))
                }
                _ => Err(ExecutionError::InternalError(format!(
                    "SQL binary operator cannot be interpreted as a unary operator"
                ))),
            },

            SQLExpr::BinaryOp {
                ref left,
                ref op,
                ref right,
            } => {
                let operator = match *op {
                    BinaryOperator::Gt => Ok(Operator::Gt),
                    BinaryOperator::GtEq => Ok(Operator::GtEq),
                    BinaryOperator::Lt => Ok(Operator::Lt),
                    BinaryOperator::LtEq => Ok(Operator::LtEq),
                    BinaryOperator::Eq => Ok(Operator::Eq),
                    BinaryOperator::NotEq => Ok(Operator::NotEq),
                    BinaryOperator::Plus => Ok(Operator::Plus),
                    BinaryOperator::Minus => Ok(Operator::Minus),
                    BinaryOperator::Multiply => Ok(Operator::Multiply),
                    BinaryOperator::Divide => Ok(Operator::Divide),
                    BinaryOperator::Modulus => Ok(Operator::Modulus),
                    BinaryOperator::And => Ok(Operator::And),
                    BinaryOperator::Or => Ok(Operator::Or),
                    BinaryOperator::Like => Ok(Operator::Like),
                    BinaryOperator::NotLike => Ok(Operator::NotLike),
                    _ => Err(ExecutionError::NotImplemented(format!(
                        "Unsupported SQL binary operator {:?}",
                        op
                    ))),
                }?;

                match operator {
                    Operator::Not => Err(ExecutionError::InternalError(format!(
                        "SQL unary operator \"NOT\" cannot be interpreted as a binary operator"
                    ))),
                    _ => Ok(Expr::BinaryExpr {
                        left: Box::new(self.sql_to_rex(&left, &schema)?),
                        op: operator,
                        right: Box::new(self.sql_to_rex(&right, &schema)?),
                    })
                }
            }

            SQLExpr::Function(function) => {
                //TODO: fix this hack
                let name: String = function.name.to_string();
                match name.to_lowercase().as_ref() {
                    "min" | "max" | "sum" | "avg" => {
                        let rex_args = function
                            .args
                            .iter()
                            .map(|a| self.sql_to_rex(a, schema))
                            .collect::<Result<Vec<Expr>>>()?;

                        // return type is same as the argument type for these aggregate
                        // functions
                        let return_type = rex_args[0].get_type(schema)?.clone();

                        Ok(Expr::AggregateFunction {
                            name: name.clone(),
                            args: rex_args,
                            return_type,
                        })
                    }
                    "count" => {
                        let rex_args = function
                            .args
                            .iter()
                            .map(|a| match a {
                                SQLExpr::Value(Value::Number(_)) => Ok(lit(1_u8)),
                                SQLExpr::Wildcard => Ok(lit(1_u8)),
                                _ => self.sql_to_rex(a, schema),
                            })
                            .collect::<Result<Vec<Expr>>>()?;

                        Ok(Expr::AggregateFunction {
                            name: name.clone(),
                            args: rex_args,
                            return_type: DataType::UInt64,
                        })
                    }
                    _ => match self.schema_provider.get_function_meta(&name) {
                        Some(fm) => {
                            let rex_args = function
                                .args
                                .iter()
                                .map(|a| self.sql_to_rex(a, schema))
                                .collect::<Result<Vec<Expr>>>()?;

                            let mut safe_args: Vec<Expr> = vec![];
                            for i in 0..rex_args.len() {
                                safe_args.push(
                                    rex_args[i]
                                        .cast_to(fm.args()[i].data_type(), schema)?,
                                );
                            }

                            Ok(Expr::ScalarFunction {
                                name: name.clone(),
                                args: safe_args,
                                return_type: fm.return_type().clone(),
                            })
                        }
                        _ => Err(ExecutionError::General(format!(
                            "Invalid function '{}'",
                            name
                        ))),
                    },
                }
            }

            _ => Err(ExecutionError::General(format!(
                "Unsupported ast node {:?} in sqltorel",
                sql
            ))),
        }
    }
}

/// Determine if an expression is an aggregate expression or not
fn is_aggregate_expr(e: &Expr) -> bool {
    match e {
        Expr::AggregateFunction { .. } => true,
        _ => false,
    }
}

/// Convert SQL data type to relational representation of data type
pub fn convert_data_type(sql: &SQLDataType) -> Result<DataType> {
    match sql {
        SQLDataType::Boolean => Ok(DataType::Boolean),
        SQLDataType::SmallInt => Ok(DataType::Int16),
        SQLDataType::Int => Ok(DataType::Int32),
        SQLDataType::BigInt => Ok(DataType::Int64),
        SQLDataType::Float(_) | SQLDataType::Real => Ok(DataType::Float64),
        SQLDataType::Double => Ok(DataType::Float64),
        SQLDataType::Char(_) | SQLDataType::Varchar(_) => Ok(DataType::Utf8),
        SQLDataType::Timestamp => Ok(DataType::Timestamp(TimeUnit::Nanosecond, None)),
        other => Err(ExecutionError::NotImplemented(format!(
            "Unsupported SQL type {:?}",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::logicalplan::FunctionType;
    use sqlparser::{dialect::GenericDialect, parser::Parser};

    #[test]
    fn select_no_relation() {
        quick_test(
            "SELECT 1",
            "Projection: Int64(1)\
             \n  EmptyRelation",
        );
    }

    #[test]
    fn select_scalar_func_with_literal_no_relation() {
        quick_test(
            "SELECT sqrt(9)",
            "Projection: sqrt(CAST(Int64(9) AS Float64))\
             \n  EmptyRelation",
        );
    }

    #[test]
    fn select_simple_selection() {
        let sql = "SELECT id, first_name, last_name \
                   FROM person WHERE state = 'CO'";
        let expected = "Projection: #id, #first_name, #last_name\
                        \n  Selection: #state Eq Utf8(\"CO\")\
                        \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_neg_selection() {
        let sql = "SELECT id, first_name, last_name \
                   FROM person WHERE NOT state";
        let expected = "Projection: #id, #first_name, #last_name\
                        \n  Selection: NOT #state\
                        \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_compound_selection() {
        let sql = "SELECT id, first_name, last_name \
                   FROM person WHERE state = 'CO' AND age >= 21 AND age <= 65";
        let expected = "Projection: #id, #first_name, #last_name\
            \n  Selection: #state Eq Utf8(\"CO\") And #age GtEq Int64(21) And #age LtEq Int64(65)\
            \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn test_timestamp_selection() {
        let sql = "SELECT state FROM person WHERE birth_date < CAST (158412331400600000 as timestamp)";

        let expected = "Projection: #state\
            \n  Selection: #birth_date Lt CAST(Int64(158412331400600000) AS Timestamp(Nanosecond, None))\
            \n    TableScan: person projection=None";

        quick_test(sql, expected);
    }

    #[test]
    fn select_all_boolean_operators() {
        let sql = "SELECT age, first_name, last_name \
                   FROM person \
                   WHERE age = 21 \
                   AND age != 21 \
                   AND age > 21 \
                   AND age >= 21 \
                   AND age < 65 \
                   AND age <= 65";
        let expected = "Projection: #age, #first_name, #last_name\
                        \n  Selection: #age Eq Int64(21) \
                        And #age NotEq Int64(21) \
                        And #age Gt Int64(21) \
                        And #age GtEq Int64(21) \
                        And #age Lt Int64(65) \
                        And #age LtEq Int64(65)\
                        \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_simple_aggregate() {
        quick_test(
            "SELECT MIN(age) FROM person",
            "Aggregate: groupBy=[[]], aggr=[[MIN(#age)]]\
             \n  TableScan: person projection=None",
        );
    }

    #[test]
    fn test_sum_aggregate() {
        quick_test(
            "SELECT SUM(age) from person",
            "Aggregate: groupBy=[[]], aggr=[[SUM(#age)]]\
             \n  TableScan: person projection=None",
        );
    }

    #[test]
    fn select_simple_aggregate_with_groupby() {
        quick_test(
            "SELECT state, MIN(age), MAX(age) FROM person GROUP BY state",
            "Aggregate: groupBy=[[#state]], aggr=[[MIN(#age), MAX(#age)]]\
             \n  TableScan: person projection=None",
        );
    }

    #[test]
    fn test_wildcard() {
        quick_test(
            "SELECT * from person",
            "Projection: #id, #first_name, #last_name, #age, #state, #salary, #birth_date\
            \n  TableScan: person projection=None",
        );
    }

    #[test]
    fn select_count_one() {
        let sql = "SELECT COUNT(1) FROM person";
        let expected = "Aggregate: groupBy=[[]], aggr=[[COUNT(UInt8(1))]]\
                        \n  TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_count_column() {
        let sql = "SELECT COUNT(id) FROM person";
        let expected = "Aggregate: groupBy=[[]], aggr=[[COUNT(#id)]]\
                        \n  TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_scalar_func() {
        let sql = "SELECT sqrt(age) FROM person";
        let expected = "Projection: sqrt(CAST(#age AS Float64))\
                        \n  TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_aliased_scalar_func() {
        let sql = "SELECT sqrt(age) AS square_people FROM person";
        let expected = "Projection: sqrt(CAST(#age AS Float64)) AS square_people\
                        \n  TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_order_by() {
        let sql = "SELECT id FROM person ORDER BY id";
        let expected = "Sort: #id ASC NULLS FIRST\
                        \n  Projection: #id\
                        \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_order_by_desc() {
        let sql = "SELECT id FROM person ORDER BY id DESC";
        let expected = "Sort: #id DESC NULLS FIRST\
                        \n  Projection: #id\
                        \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_order_by_nulls_last() {
        quick_test(
            "SELECT id FROM person ORDER BY id DESC NULLS LAST",
            "Sort: #id DESC NULLS LAST\
            \n  Projection: #id\
            \n    TableScan: person projection=None",
        );

        quick_test(
            "SELECT id FROM person ORDER BY id NULLS LAST",
            "Sort: #id ASC NULLS LAST\
            \n  Projection: #id\
            \n    TableScan: person projection=None",
        );
    }

    #[test]
    fn select_group_by() {
        let sql = "SELECT state FROM person GROUP BY state";
        let expected = "Aggregate: groupBy=[[#state]], aggr=[[]]\
                        \n  TableScan: person projection=None";

        quick_test(sql, expected);
    }

    #[test]
    fn select_group_by_needs_projection() {
        let sql = "SELECT COUNT(state), state FROM person GROUP BY state";
        let expected = "\
        Projection: #COUNT(state), #state\
        \n  Aggregate: groupBy=[[#state]], aggr=[[COUNT(#state)]]\
        \n    TableScan: person projection=None";

        quick_test(sql, expected);
    }

    #[test]
    fn select_7480_1() {
        let sql = "SELECT c1, MIN(c12) FROM aggregate_test_100 GROUP BY c1, c13";
        let err = logical_plan(sql).expect_err("query should have failed");
        assert_eq!(
            "General(\"Projection references non-aggregate values\")",
            format!("{:?}", err)
        );
    }

    #[test]
    fn select_7480_2() {
        let sql = "SELECT c1, c13, MIN(c12) FROM aggregate_test_100 GROUP BY c1";
        let err = logical_plan(sql).expect_err("query should have failed");
        assert_eq!(
            "General(\"Projection references non-aggregate values\")",
            format!("{:?}", err)
        );
    }

    fn logical_plan(sql: &str) -> Result<LogicalPlan> {
        let dialect = GenericDialect {};
        let planner = SqlToRel::new(MockSchemaProvider {});
        let ast = Parser::parse_sql(&dialect, sql).unwrap();
        planner.statement_to_plan(&ast[0])
    }

    /// Create logical plan, write with formatter, compare to expected output
    fn quick_test(sql: &str, expected: &str) {
        let plan = logical_plan(sql).unwrap();
        assert_eq!(expected, format!("{:?}", plan));
    }

    struct MockSchemaProvider {}

    impl SchemaProvider for MockSchemaProvider {
        fn get_table_meta(&self, name: &str) -> Option<SchemaRef> {
            match name {
                "person" => Some(Arc::new(Schema::new(vec![
                    Field::new("id", DataType::UInt32, false),
                    Field::new("first_name", DataType::Utf8, false),
                    Field::new("last_name", DataType::Utf8, false),
                    Field::new("age", DataType::Int32, false),
                    Field::new("state", DataType::Utf8, false),
                    Field::new("salary", DataType::Float64, false),
                    Field::new(
                        "birth_date",
                        DataType::Timestamp(TimeUnit::Nanosecond, None),
                        false,
                    ),
                ]))),
                "aggregate_test_100" => Some(Arc::new(Schema::new(vec![
                    Field::new("c1", DataType::Utf8, false),
                    Field::new("c2", DataType::UInt32, false),
                    Field::new("c3", DataType::Int8, false),
                    Field::new("c4", DataType::Int16, false),
                    Field::new("c5", DataType::Int32, false),
                    Field::new("c6", DataType::Int64, false),
                    Field::new("c7", DataType::UInt8, false),
                    Field::new("c8", DataType::UInt16, false),
                    Field::new("c9", DataType::UInt32, false),
                    Field::new("c10", DataType::UInt64, false),
                    Field::new("c11", DataType::Float32, false),
                    Field::new("c12", DataType::Float64, false),
                    Field::new("c13", DataType::Utf8, false),
                ]))),
                _ => None,
            }
        }

        fn get_function_meta(&self, name: &str) -> Option<Arc<FunctionMeta>> {
            match name {
                "sqrt" => Some(Arc::new(FunctionMeta::new(
                    "sqrt".to_string(),
                    vec![Field::new("n", DataType::Float64, false)],
                    DataType::Float64,
                    FunctionType::Scalar,
                ))),
                _ => None,
            }
        }
    }
}
