//! Shapes built from lines.

mod boxes;

pub use boxes::{
    body_row, bordered_box, bottom_rule, interior, rule, rule_with_lead, top_rule, BoxSpec,
    MIN_WIDTH,
};
