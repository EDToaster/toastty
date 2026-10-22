//! CSI / DCS parameter list, the same shape and API as `vte::Params`.
//!
//! The scanner parses common CSI sequences itself (see
//! `Scanner::fast_csi`): vte's per-byte state machine was most of the
//! time spent on SGR-heavy output. `vte::Params` can only be built by
//! vte, so handlers get this type instead, and parameters vte parsed (DCS
//! hooks, CSIs the fast path leaves to vte) are copied into it.

use core::fmt::{self, Debug, Formatter};

/// Most parameters plus subparameters one sequence carries, as in vte.
pub(crate) const MAX_PARAMS: usize = 32;

/// Parameters with optional subparameters (`CSI 38:2::1:2:3 m`).
/// [`Params::iter`] yields one slice per parameter: the value, then its
/// colon-separated subparameters.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct Params {
    /// For the first entry of each parameter, how many entries it spans
    /// (1 + subparameters); 0 at subparameter positions.
    subparams: [u8; MAX_PARAMS],
    /// All parameters and subparameters.
    values: [u16; MAX_PARAMS],
    /// Subparameters in the current parameter so far.
    current_subparams: u8,
    /// Total number of parameters and subparameters.
    len: usize,
}

impl Params {
    /// Number of parameters and subparameters.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True if there are none.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Iterate the parameters, each with its subparameters.
    #[inline]
    #[must_use]
    pub fn iter(&self) -> ParamsIter<'_> {
        ParamsIter {
            params: self,
            index: 0,
        }
    }

    /// True if no more entries fit.
    #[inline]
    pub(crate) fn is_full(&self) -> bool {
        self.len == MAX_PARAMS
    }

    /// Add `item` as the last value of the current parameter (a `;` or
    /// the final byte follows it). Caller checks [`Self::is_full`].
    #[inline]
    pub(crate) fn push(&mut self, item: u16) {
        self.subparams[self.len - self.current_subparams as usize] = self.current_subparams + 1;
        self.values[self.len] = item;
        self.current_subparams = 0;
        self.len += 1;
    }

    /// Add `item` to the current parameter with more to come (a `:`
    /// follows it). Caller checks [`Self::is_full`].
    #[inline]
    pub(crate) fn extend(&mut self, item: u16) {
        self.subparams[self.len - self.current_subparams as usize] = self.current_subparams + 1;
        self.values[self.len] = item;
        self.current_subparams += 1;
        self.len += 1;
    }

    /// Copy parameters vte parsed.
    pub(crate) fn from_vte(p: &vte::Params) -> Self {
        let mut out = Self::default();
        for param in p {
            if let Some((&last, head)) = param.split_last() {
                for &v in head {
                    out.extend(v);
                }
                out.push(last);
            }
        }
        out
    }
}

impl<'a> IntoIterator for &'a Params {
    type IntoIter = ParamsIter<'a>;
    type Item = &'a [u16];

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over [`Params`]: one slice per parameter.
#[derive(Debug)]
pub struct ParamsIter<'a> {
    params: &'a Params,
    index: usize,
}

impl<'a> Iterator for ParamsIter<'a> {
    type Item = &'a [u16];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.params.len {
            return None;
        }
        let n = self.params.subparams[self.index] as usize;
        let param = &self.params.values[self.index..self.index + n];
        self.index += n;
        Some(param)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.params.len - self.index;
        (remaining, Some(remaining))
    }
}

impl Debug for Params {
    /// `[1;38:5:2]`, as vte prints it.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        for (i, param) in self.iter().enumerate() {
            if i != 0 {
                write!(f, ";")?;
            }
            for (j, sub) in param.iter().enumerate() {
                if j != 0 {
                    write!(f, ":")?;
                }
                sub.fmt(f)?;
            }
        }
        write!(f, "]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(p: &Params) -> Vec<Vec<u16>> {
        p.iter().map(<[u16]>::to_vec).collect()
    }

    #[test]
    fn push_and_extend_group_subparams() {
        let mut p = Params::default();
        assert!(p.is_empty());
        // `1;38:2:10;0`, fed the way vte's actions do.
        p.push(1);
        p.extend(38);
        p.extend(2);
        p.push(10);
        p.push(0);
        assert_eq!(collect(&p), vec![vec![1], vec![38, 2, 10], vec![0]]);
        assert_eq!(p.len(), 5);
        assert_eq!(format!("{p:?}"), "[1;38:2:10;0]");
    }

    #[test]
    fn copies_vte_params() {
        struct Grab(Option<Params>);
        impl vte::Perform for Grab {
            fn csi_dispatch(&mut self, p: &vte::Params, _: &[u8], _: bool, _: char) {
                self.0 = Some(Params::from_vte(p));
            }
        }
        let mut g = Grab(None);
        vte::Parser::new().advance(&mut g, b"\x1b[4:3;;38:2::1:2:3m");
        let p = g.0.unwrap();
        assert_eq!(
            collect(&p),
            vec![vec![4, 3], vec![0], vec![38, 2, 0, 1, 2, 3]]
        );
    }
}
