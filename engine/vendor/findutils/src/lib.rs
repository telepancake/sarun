// Copyright 2017 Google Inc.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

// sarun: reduced to a find-and-xargs library. Upstream's `locate`, `updatedb`,
// and `testing` modules (and their binaries) are dropped — this fork vendors
// findutils solely to run `find` and `xargs` as in-process brush builtins.
pub mod find;
pub mod xargs;
