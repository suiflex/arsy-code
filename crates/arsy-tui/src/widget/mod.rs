//! Shapes built from lines.

mod boxes;
mod card;

pub use boxes::{
    body_row, bordered_box, bottom_rule, interior, rule, rule_with_lead, top_rule, BoxSpec,
    MIN_WIDTH,
};
pub use card::{card, CardSpec};
