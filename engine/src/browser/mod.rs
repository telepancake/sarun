// cellulose — the in-engine textmode browser (DESIGN-cellulose.md).
//
// A renderer that drives a stock headless Chromium over the DevTools Protocol
// and composes its pages into a terminal cell grid, replacing the vendored
// carbonyl fork. This module is the engine-side port of the `cellulose/`
// Python prototype.
//
// Increment A: the CDP client + transport (`cdp`). Increment B: the synthetic
// cell `font` and the DOMSnapshot→cell-grid `render`er. Increment C adds
// `session` per the design doc's C5 ladder.

pub mod cdp;
pub mod font;
pub mod render;
pub mod session;
