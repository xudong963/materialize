// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context};
use aws_util::aws;
use rusoto_core::Region;

use repr::ColumnName;
use sql_parser::ast::display::AstDisplay;
use sql_parser::ast::visit_mut::{self, VisitMut};
use sql_parser::ast::{
    AstInfo, CreateIndexStatement, CreateSinkStatement, CreateSourceStatement,
    CreateTableStatement, CreateTypeStatement, CreateViewStatement, DataType, Function,
    FunctionArgs, Ident, IfExistsBehavior, ObjectName, Query, Raw, SqlOption, Statement,
    TableFactor, Value,
};

use crate::names::{DatabaseSpecifier, FullName, PartialName};
use crate::plan::error::PlanError;
use crate::plan::query;
use crate::plan::statement::StatementContext;

pub fn ident(ident: Ident) -> String {
    ident.as_str().into()
}

pub fn column_name(id: Ident) -> ColumnName {
    ColumnName::from(ident(id))
}

pub fn object_name(mut name: ObjectName) -> Result<PartialName, PlanError> {
    if name.0.len() < 1 || name.0.len() > 3 {
        return Err(PlanError::MisqualifiedName(name.to_string()));
    }
    let out = PartialName {
        item: ident(
            name.0
                .pop()
                .expect("name checked to have at least one component"),
        ),
        schema: name.0.pop().map(ident),
        database: name.0.pop().map(ident),
    };
    assert!(name.0.is_empty());
    Ok(out)
}

pub fn options(options: &[SqlOption]) -> BTreeMap<String, Value> {
    options
        .iter()
        .map(|o| match o {
            SqlOption::Value { name, value } => (ident(name.clone()), value.clone()),
            SqlOption::ObjectName { name, object_name } => (
                ident(name.clone()),
                Value::String(object_name.to_ast_string()),
            ),
            SqlOption::DataType { name, data_type } => (
                ident(name.clone()),
                Value::String(data_type.to_ast_string()),
            ),
        })
        .collect()
}

pub fn option_objects(options: &[SqlOption]) -> BTreeMap<String, SqlOption> {
    options
        .iter()
        .map(|o| (ident(o.name().clone()), o.clone()))
        .collect()
}

pub fn unresolve(name: FullName) -> ObjectName {
    let mut out = vec![];
    if let DatabaseSpecifier::Name(n) = name.database {
        out.push(Ident::new(n));
    }
    out.push(Ident::new(name.schema));
    out.push(Ident::new(name.item));
    ObjectName(out)
}

/// Normalizes a `CREATE { SOURCE | VIEW | INDEX | SINK }` statement so that the
/// statement does not depend upon any session parameters, nor specify any
/// non-default options (like `MATERIALIZED`, `IF NOT EXISTS`, etc).
///
/// The goal is to construct a backwards-compatible description of the object.
/// SQL is the most stable part of Materialize, so SQL is used to describe the
/// objects that are persisted in the catalog.
pub fn create_statement(
    scx: &StatementContext,
    mut stmt: Statement<Raw>,
) -> Result<String, PlanError> {
    let allocate_name = |name: &ObjectName| -> Result<_, PlanError> {
        Ok(unresolve(scx.allocate_name(object_name(name.clone())?)))
    };

    let allocate_temporary_name = |name: &ObjectName| -> Result<_, PlanError> {
        Ok(unresolve(
            scx.allocate_temporary_name(object_name(name.clone())?),
        ))
    };

    let resolve_item = |name: &ObjectName| -> Result<_, PlanError> {
        let item = scx.resolve_item(name.clone())?;
        Ok(unresolve(item.name().clone()))
    };

    struct QueryNormalizer<'a> {
        scx: &'a StatementContext<'a>,
        ctes: Vec<Ident>,
        err: Option<PlanError>,
    }

    impl<'a> QueryNormalizer<'a> {
        fn new(scx: &'a StatementContext<'a>) -> QueryNormalizer<'a> {
            QueryNormalizer {
                scx,
                ctes: vec![],
                err: None,
            }
        }
    }

    impl<'a, 'ast> VisitMut<'ast, Raw> for QueryNormalizer<'a> {
        fn visit_query_mut(&mut self, query: &'ast mut Query<Raw>) {
            let n = self.ctes.len();
            for cte in &query.ctes {
                self.ctes.push(cte.alias.name.clone());
            }
            visit_mut::visit_query_mut(self, query);
            self.ctes.truncate(n);
        }

        fn visit_function_mut(&mut self, func: &'ast mut Function<Raw>) {
            self.visit_object_name_mut(&mut func.name);

            match &mut func.args {
                FunctionArgs::Star => (),
                FunctionArgs::Args(args) => {
                    for arg in args {
                        self.visit_expr_mut(arg);
                    }
                }
            }
            if let Some(over) = &mut func.over {
                self.visit_window_spec_mut(over);
            }
        }

        fn visit_table_factor_mut(&mut self, table_factor: &'ast mut TableFactor<Raw>) {
            match table_factor {
                TableFactor::Table { name, alias } => {
                    self.visit_object_name_mut(name);
                    if let Some(alias) = alias {
                        self.visit_table_alias_mut(alias);
                    }
                }
                TableFactor::Function { name, args, alias } => {
                    self.visit_object_name_mut(name);
                    match args {
                        FunctionArgs::Star => (),
                        FunctionArgs::Args(args) => {
                            for expr in args {
                                self.visit_expr_mut(expr);
                            }
                        }
                    }
                    if let Some(alias) = alias {
                        self.visit_table_alias_mut(alias);
                    }
                }
                // We only need special behavior for `TableFactor::Table` and
                // `TableFactor::Function`. Just visit the other types of table
                // factors like normal.
                _ => visit_mut::visit_table_factor_mut(self, table_factor),
            }
        }

        fn visit_object_name_mut(&mut self, object_name: &'ast mut ObjectName) {
            // Single-part object names can refer to CTEs in addition to
            // catalog objects.
            if let [ident] = object_name.0.as_slice() {
                if self.ctes.contains(ident) {
                    return;
                }
            }
            match self.scx.resolve_item(object_name.clone()) {
                Ok(full_name) => *object_name = unresolve(full_name.name().clone()),
                Err(e) => self.err = Some(e),
            };
        }

        fn visit_table_mut(&mut self, table: &'ast mut <Raw as AstInfo>::Table) {
            self.visit_object_name_mut(table);
        }

        fn visit_data_type_mut(&mut self, data_type: &'ast mut DataType) {
            if let DataType::Other { name, typ_mod } = data_type {
                let canonical_name = query::canonicalize_type_name_internal(&name.clone());
                if &canonical_name != name {
                    // None of our underlying types with aliases support
                    // typ_mods, while the aliases themselves do, so we should
                    // ensure they're empty.
                    *typ_mod = vec![];
                    *name = canonical_name;
                }
            }
            visit_mut::visit_data_type_mut(self, data_type)
        }
    }

    // Think very hard before changing any of the branches in this match
    // statement. All identifiers must be quoted. All object names must be
    // allocated or resolved, depending on whether they are the object created
    // by the statement or an object depended upon by the statement. All
    // non-default options must be disabled.
    //
    // Wildcard matches are explicitly avoided so that future additions to the
    // syntax cause compile errors here. Before you ignore a new field, triple
    // check that it does not need to be normalized according to the rules
    // above.
    match &mut stmt {
        Statement::CreateSource(CreateSourceStatement {
            name,
            col_names: _,
            connector: _,
            with_options: _,
            format: _,
            envelope: _,
            if_not_exists,
            materialized,
        }) => {
            *name = allocate_name(name)?;
            *if_not_exists = false;
            *materialized = false;
        }

        Statement::CreateTable(CreateTableStatement {
            name,
            columns,
            constraints: _,
            with_options: _,
            if_not_exists,
        }) => {
            *name = allocate_name(name)?;
            let mut normalizer = QueryNormalizer::new(scx);
            for c in columns {
                normalizer.visit_column_def_mut(c);
            }
            if let Some(err) = normalizer.err {
                return Err(err);
            }
            *if_not_exists = false;
        }

        Statement::CreateSink(CreateSinkStatement {
            name,
            from,
            connector: _,
            with_options: _,
            format: _,
            envelope: _,
            with_snapshot: _,
            as_of: _,
            if_not_exists,
        }) => {
            *name = allocate_name(name)?;
            *from = resolve_item(from)?;
            *if_not_exists = false;
        }

        Statement::CreateView(CreateViewStatement {
            name,
            columns: _,
            query,
            temporary,
            materialized,
            if_exists,
            with_options: _,
        }) => {
            *name = if *temporary {
                allocate_temporary_name(name)?
            } else {
                allocate_name(name)?
            };
            {
                let mut normalizer = QueryNormalizer::new(scx);
                normalizer.visit_query_mut(query);
                if let Some(err) = normalizer.err {
                    return Err(err);
                }
            }
            *materialized = false;
            *if_exists = IfExistsBehavior::Error;
        }

        Statement::CreateIndex(CreateIndexStatement {
            name: _,
            on_name,
            key_parts,
            if_not_exists,
        }) => {
            *on_name = resolve_item(on_name)?;
            let mut normalizer = QueryNormalizer::new(scx);
            if let Some(key_parts) = key_parts {
                for key_part in key_parts {
                    normalizer.visit_expr_mut(key_part);
                    if let Some(err) = normalizer.err {
                        return Err(err);
                    }
                }
            }
            *if_not_exists = false;
        }

        Statement::CreateType(CreateTypeStatement {
            name, with_options, ..
        }) => {
            *name = allocate_name(name)?;
            let mut normalizer = QueryNormalizer::new(scx);
            for option in with_options {
                match option {
                    SqlOption::DataType { data_type, .. } => {
                        normalizer.visit_data_type_mut(data_type);
                    }
                    _ => unreachable!(),
                }
            }
            if let Some(err) = normalizer.err {
                return Err(err);
            }
        }

        _ => unreachable!(),
    }

    Ok(stmt.to_ast_string_stable())
}

macro_rules! with_option_type {
    ($name:ident, String) => {
        if let Some(crate::ast::WithOptionValue::Value(crate::ast::Value::String(value))) = $name {
            value
        } else if let Some(crate::ast::WithOptionValue::ObjectName(name)) = $name {
            crate::ast::display::AstDisplay::to_ast_string(&name)
        } else {
            ::anyhow::bail!("expected String");
        }
    };
    ($name:ident, bool) => {
        if let Some(crate::ast::WithOptionValue::Value(crate::ast::Value::Boolean(value))) = $name {
            value
        } else if $name.is_none() {
            // Bools, if they have no '= value', are true.
            true
        } else {
            ::anyhow::bail!("expected bool");
        }
    };
    ($name:ident, Interval) => {
        if let Some(crate::ast::WithOptionValue::Value(Value::String(value))) = $name {
            ::repr::strconv::parse_interval(&value)?
        } else if let Some(crate::ast::WithOptionValue::Value(Value::Interval(interval))) = $name {
            ::repr::strconv::parse_interval(&interval.value)?
        } else {
            ::anyhow::bail!("expected Interval");
        }
    };
}

/// This macro accepts a struct definition and will generate it and a `try_from`
/// method that takes a `Vec<WithOption>` which will extract and type check
/// options based on the struct field names and types. Field names must match
/// exactly the lowercased option name. Supported types are:
///
/// - `String`: expects a SQL string (`WITH (name = "value")`) or identifier
///   (`WITH (name = text)`).
/// - `bool`: expects either a SQL bool (`WITH (name = true)`) or a valueless
///   option which will be interpreted as true: (`WITH (name)`.
/// - `Interval`: expects either a SQL interval or string that can be parsed as
///   an interval.
macro_rules! with_options {
  (struct $name:ident {
        $($field_name:ident: $field_type:ident,)*
    }) => {
        #[derive(Debug)]
        pub struct $name {
            pub $($field_name: Option<$field_type>,)*
        }

        impl ::std::convert::TryFrom<Vec<crate::ast::WithOption>> for $name {
            type Error = anyhow::Error;

            fn try_from(mut options: Vec<crate::ast::WithOption>) -> Result<Self, Self::Error> {
                let v = Self {
                    $($field_name: {
                        match options.iter().position(|opt| opt.key.as_str() == stringify!($field_name)) {
                            None => None,
                            Some(pos) => {
                                let value: Option<crate::ast::WithOptionValue> = options.swap_remove(pos).value;
                                let value: $field_type = with_option_type!(value, $field_type);
                                Some(value)
                            },
                        }
                    },
                    )*
                };
                if !options.is_empty() {
                    ::anyhow::bail!("unexpected options");
                }
                Ok(v)
            }
        }
    }
}

pub fn aws_connect_info(
    options: &mut BTreeMap<String, Value>,
    region: Option<String>,
) -> anyhow::Result<aws::ConnectInfo> {
    let mut extract = |key| match options.remove(key) {
        Some(Value::String(key)) => {
            if !key.is_empty() {
                Ok(Some(key))
            } else {
                Ok(None)
            }
        }
        Some(_) => bail!("{} must be a string", key),
        _ => Ok(None),
    };

    let region_raw = match region {
        Some(region) => region,
        None => extract("region")?.ok_or_else(|| anyhow!("region is required"))?,
    };

    let region = match region_raw.parse() {
        Ok(region) => {
            // ignore/drop the endpoint option if we're pointing at a valid,
            // non-custom AWS region. Endpoints are meaningless without custom
            // regions, and this makes writing tests that support both
            // LocalStack and real AWS much easier.
            let _ = extract("endpoint");
            region
        }
        Err(e) => {
            // Region's FromStr doesn't support parsing custom regions.
            // If a Kinesis stream's ARN indicates it exists in a custom
            // region, support it iff a valid endpoint for the stream
            // is also provided.
            match extract("endpoint").with_context(|| {
                format!("endpoint is required for custom regions: {:?}", region_raw)
            })? {
                Some(endpoint) => Region::Custom {
                    name: region_raw,
                    endpoint,
                },
                _ => bail!(
                    "Unable to parse AWS region: {}. If providing a custom \
                     region, an `endpoint` option must also be provided",
                    e
                ),
            }
        }
    };

    aws::ConnectInfo::new(
        region,
        extract("access_key_id")?,
        extract("secret_access_key")?,
        extract("token")?,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::rc::Rc;

    use ore::collections::CollectionExt;

    use super::*;
    use crate::catalog::DummyCatalog;
    use crate::plan::PlanContext;

    #[test]
    fn normalized_create() -> Result<(), Box<dyn Error>> {
        let scx = &StatementContext {
            pcx: &PlanContext::default(),
            catalog: &DummyCatalog,
            param_types: Rc::new(RefCell::new(BTreeMap::new())),
        };

        let parsed = sql_parser::parser::parse_statements(
            "create materialized view foo as select 1 as bar",
        )?
        .into_element();

        // Ensure that all identifiers are quoted.
        assert_eq!(
            r#"CREATE VIEW "dummy"."public"."foo" AS SELECT 1 AS "bar""#,
            create_statement(scx, parsed)?,
        );

        Ok(())
    }

    #[test]
    fn with_options_errors_if_endpoint_missing_for_invalid_region() {
        let mut map = BTreeMap::new();
        map.insert("region".to_string(), Value::String("nonsense".into()));
        assert!(aws_connect_info(&mut map, None).is_err());

        let mut map = BTreeMap::new();
        assert!(aws_connect_info(&mut map, Some("nonsense".into())).is_err());
    }

    #[test]
    fn with_options_allows_invalid_region_with_endpoint() {
        let mut map = BTreeMap::new();
        map.insert("region".to_string(), Value::String("nonsense".into()));
        map.insert("endpoint".to_string(), Value::String("endpoint".into()));
        assert!(aws_connect_info(&mut map, None).is_ok());

        let mut map = BTreeMap::new();
        map.insert("endpoint".to_string(), Value::String("endpoint".into()));
        assert!(aws_connect_info(&mut map, Some("nonsense".into())).is_ok());
    }

    #[test]
    fn with_options_ignores_endpoint_with_valid_region() {
        let mut map = BTreeMap::new();
        map.insert("region".to_string(), Value::String("us-east-1".into()));
        map.insert("endpoint".to_string(), Value::String("endpoint".into()));
        assert!(aws_connect_info(&mut map, None).is_ok());

        let mut map = BTreeMap::new();
        map.insert("endpoint".to_string(), Value::String("endpoint".into()));
        assert!(aws_connect_info(&mut map, Some("us-east-1".into())).is_ok());

        let mut map = BTreeMap::new();
        map.insert("region".to_string(), Value::String("us-east-1".into()));
        assert!(aws_connect_info(&mut map, None).is_ok());

        let mut map = BTreeMap::new();
        assert!(aws_connect_info(&mut map, Some("us-east-1".into())).is_ok());
    }
}
