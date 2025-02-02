/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use buck2_node::attrs::attr_type::any::AnyAttrType;
use buck2_node::attrs::attr_type::attr_literal::AttrLiteral;
use buck2_node::attrs::attr_type::attr_literal::ListLiteral;
use buck2_node::attrs::attr_type::AttrType;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::coercion_context::AttrCoercionContext;
use buck2_node::attrs::configurable::AttrIsConfigurable;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::tuple::TupleRef;
use starlark::values::Value;

use crate::attrs::coerce::AttrTypeCoerce;

fn to_coerced_literal(value: Value) -> CoercedAttr {
    CoercedAttr::Literal(to_literal(value))
}

fn to_literal(value: Value) -> AttrLiteral<CoercedAttr> {
    if value.is_none() {
        AttrLiteral::None
    } else if let Some(x) = value.unpack_bool() {
        AttrLiteral::Bool(x)
    } else if let Some(x) = value.unpack_int() {
        AttrLiteral::Int(x)
    } else if let Some(x) = DictRef::from_value(value) {
        AttrLiteral::Dict(
            x.iter()
                .map(|(k, v)| (to_coerced_literal(k), to_coerced_literal(v)))
                .collect(),
        )
    } else if let Some(x) = TupleRef::from_value(value) {
        AttrLiteral::Tuple(x.iter().map(to_coerced_literal).collect())
    } else if let Some(x) = ListRef::from_value(value) {
        AttrLiteral::List(box ListLiteral {
            items: x.iter().map(to_coerced_literal).collect(),
            item_type: AttrType::any(),
        })
    } else {
        AttrLiteral::String(value.to_str().into_boxed_str())
    }
}

impl AttrTypeCoerce for AnyAttrType {
    fn coerce_item(
        &self,
        _configurable: AttrIsConfigurable,
        _ctx: &dyn AttrCoercionContext,
        value: Value,
    ) -> anyhow::Result<AttrLiteral<CoercedAttr>> {
        Ok(to_literal(value))
    }

    fn starlark_type(&self) -> String {
        "\"\"".to_owned()
    }
}
