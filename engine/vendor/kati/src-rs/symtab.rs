/*
Copyright 2025 Google LLC

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::{
    collections::HashMap,
    fmt::{Debug, Display},
    num::NonZeroUsize,
    sync::{Arc, LazyLock},
    vec,
};

use crate::var::Var;
use anyhow::Result;
use bytes::{BufMut, Bytes, BytesMut};
use parking_lot::Mutex;

static SYMTAB: LazyLock<Mutex<Symtab>> = LazyLock::new(|| Mutex::new(Symtab::new()));

pub static SHELL_SYM: LazyLock<Symbol> = LazyLock::new(|| intern("SHELL"));
pub static ALLOW_RULES_SYM: LazyLock<Symbol> = LazyLock::new(|| intern(".KATI_ALLOW_RULES"));
pub static KATI_READONLY_SYM: LazyLock<Symbol> = LazyLock::new(|| intern(".KATI_READONLY"));
pub static VARIABLES_SYM: LazyLock<Symbol> = LazyLock::new(|| intern(".VARIABLES"));
pub static KATI_SYMBOLS_SYM: LazyLock<Symbol> = LazyLock::new(|| intern(".KATI_SYMBOLS"));
pub static MAKEFILE_LIST: LazyLock<Symbol> = LazyLock::new(|| intern("MAKEFILE_LIST"));

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Symbol(NonZeroUsize);

impl Display for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let r = SYMTAB.lock();
        write!(f, "{}", String::from_utf8_lossy(&r.symbols[self.0.get()]))
    }
}

impl Debug for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let r = SYMTAB.lock();
        write!(f, "{:?}({})", r.symbols[self.0.get()], self.0.get())
    }
}

impl Symbol {
    pub fn as_bytes(&self) -> Bytes {
        let r = SYMTAB.lock();
        r.symbols[self.0.get()].clone()
    }

    /// sarun: the interned id, used to index an Evaluator's per-instance
    /// global-variable store (see Evaluator::global_vars). The variable
    /// bindings used to live here in the symtab; they now live per-Evaluator.
    pub fn index(&self) -> usize {
        self.0.get()
    }

    /// sarun: rebuild a Symbol from an id produced by `index()`. None for id 0
    /// (the reserved empty symbol).
    pub fn from_index(idx: usize) -> Option<Symbol> {
        Some(Symbol(NonZeroUsize::new(idx)?))
    }
}

/// sarun: a temporary override of a global variable binding, restored on Drop.
/// Operates on an Evaluator's per-instance store (an Arc<Mutex<…>> handle to
/// Evaluator::global_vars) so the Drop restore needs no &Evaluator — which is
/// what lets it survive `?`-early-returns. Used for $@/$(D)/$(F) under
/// .SECONDEXPANSION and for foreach/call temp vars.
pub struct ScopedGlobalVar {
    store: Arc<Mutex<Vec<Option<Var>>>>,
    idx: usize,
    orig: Option<Var>,
}

impl ScopedGlobalVar {
    pub fn new(store: Arc<Mutex<Vec<Option<Var>>>>, sym: Symbol, var: Var) -> Result<Self> {
        let idx = sym.index();
        let orig = {
            let mut s = store.lock();
            if idx >= s.len() {
                s.resize(idx + 1, None);
            }
            let orig = s[idx].clone();
            s[idx] = Some(var);
            orig
        };
        Ok(Self { store, idx, orig })
    }
}

impl Drop for ScopedGlobalVar {
    fn drop(&mut self) {
        let mut s = self.store.lock();
        if self.idx < s.len() {
            s[self.idx] = self.orig.clone();
        }
    }
}

struct Symtab {
    symbols: Vec<Bytes>,
    symtab: HashMap<Bytes, Symbol>,
}

impl Symtab {
    fn new() -> Self {
        let mut symtab = Self {
            symbols: vec![Bytes::new()],
            symtab: HashMap::new(),
        };
        for i in 1u8..=255 {
            assert!(symtab.symbols.len() == i as usize);
            let name = Bytes::from(vec![i]);
            let sym = Symbol(NonZeroUsize::new(i.into()).unwrap());
            symtab.symbols.push(name.clone());
            symtab.symtab.insert(name, sym);
        }
        // sarun: the builtin special vars (.SHELLSTATUS/.VARIABLES/.KATI_SYMBOLS)
        // are no longer seeded here. Variable bindings are per-Evaluator now, so
        // Evaluator::seed_special_vars installs them; the symtab is purely the
        // name<->id interner.
        symtab
    }

    fn intern<T: Into<Bytes> + AsRef<[u8]>>(&mut self, s: T) -> Symbol {
        if let [c] = s.as_ref() {
            return Symbol(NonZeroUsize::new(*c as usize).unwrap());
        }
        let s = s.into();
        if let Some(sym) = self.symtab.get(&s) {
            return *sym;
        }
        let sym = Symbol(NonZeroUsize::new(self.symbols.len()).unwrap());
        self.symbols.push(s.clone());
        self.symtab.insert(s, sym);
        sym
    }

}

pub fn intern<T: Into<Bytes> + AsRef<[u8]>>(s: T) -> Symbol {
    let mut w = SYMTAB.lock();
    w.intern(s)
}

pub fn join_symbols(symbols: &[Symbol], sep: &[u8]) -> Bytes {
    let mut r = BytesMut::new();
    let mut first = true;
    for s in symbols {
        if !first {
            r.put_slice(sep);
        } else {
            first = false;
        }
        r.put_slice(&s.as_bytes());
    }
    r.freeze()
}

pub fn symbol_count() -> usize {
    let s = SYMTAB.lock();
    s.symbols.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intern() {
        let sym = intern("foo");
        let sym2 = intern("bar");
        let sym3 = intern("foo");
        assert_ne!(sym, sym2);
        assert_eq!(sym, sym3);
    }

    #[test]
    fn test_symbol_to_string() {
        let sym = intern("foo");
        assert_eq!(sym.to_string(), "foo");
    }

    #[test]
    fn test_single_letter_symbol() {
        let sym = intern("a");
        assert_eq!(sym.0.get(), 'a' as usize);
    }
}
