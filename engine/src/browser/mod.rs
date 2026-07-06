// cellulose — the in-engine textmode browser (DESIGN-cellulose.md).
//
// A renderer that drives a stock headless Chromium over the DevTools Protocol
// and composes its pages into a terminal cell grid, replacing the vendored
// carbonyl fork. This module is the engine-side port of the `cellulose/`
// Python prototype.
//
// Increment A (here): the CDP client + transport (`cdp`). Later increments add
// `font`, `render`, and `session` per the design doc's C5 ladder.

pub mod cdp;
