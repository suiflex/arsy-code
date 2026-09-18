//! The ARSY terminal presentation layer.
//!
//! This crate holds what a row *is* and what it looks like, and nothing about
//! where it goes. It opens no file, touches no terminal, reads no clock and
//! depends on no other crate in the workspace — so every widget in it renders
//! the same string twice for the same input, which is what makes a snapshot
//! test of the interface possible at all.
//!
//! The pieces, bottom up:
//!
//! - [`Role`] and [`Style`] say what a piece of text means.
//! - [`Span`] and [`Line`] carry text that stays measurable until it is
//!   serialised, which is what lets a row be wrapped, cut, or re-themed after
//!   it is built. The old renderer produced pre-escaped strings and could do
//!   none of those things.
//! - [`Palette`] decides what a role looks like, with `[theme]` overrides
//!   applied in one place rather than at every call site.
//! - [`wrap`] breaks a row at a width, which the renderer has never had.
//! - [`widget`] holds the shapes built from all of it.
//!
//! Terminal lifecycle — raw mode, size, key decoding — stays in the CLI. It is
//! I/O, not presentation, and putting it here would cost this crate the
//! property that makes it testable.

pub mod line;
pub mod palette;
pub mod style;
pub mod widget;
pub mod wrap;

pub use line::{Line, Span};
pub use palette::{builtin_palette, hex_to_sgr, Palette, DEFAULT_THEME, THEMES, THEME_ROLES};
pub use style::{Role, Style};
pub use widget::bordered_box;
pub use wrap::wrap;
