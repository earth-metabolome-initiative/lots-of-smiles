//! The cost-ordered [`FilterEngine`].
//!
//! Compiled once from a [`Filters`] value and shared (`Sync`) across staging
//! threads, the engine evaluates each molecule in increasing order of cost and
//! short-circuits on the first failing check:
//!
//! 1. zero-allocation byte scans ([`prescan`]),
//! 2. a shipped-column mass fast path (when a source supplies a usable mass),
//! 3. parser-backed checks ([`parsed`]) that parse the SMILES exactly once.

pub mod parsed;
pub mod prescan;

use smiles_parser::prelude::Smiles;

use crate::config::filters::{Filters, MassKind};

/// Why a molecule was rejected (for diagnostics and stats bucketing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// More than one connected component.
    MultipleComponents,
    /// Contains an explicit isotope.
    Isotope,
    /// Contains a radical / unsatisfied valence.
    Radical,
    /// Atom count outside the configured bounds.
    AtomCount,
    /// Contains an element outside the whitelist / inside the blacklist.
    Composition,
    /// Net formal charge magnitude exceeds the bound.
    Charge,
    /// Molecular mass outside the configured bounds.
    Mass,
}

/// Outcome of evaluating one molecule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOutcome {
    /// The molecule passed every configured check.
    Pass,
    /// The molecule was rejected by the named check.
    Reject(RejectReason),
    /// The SMILES could not be parsed.
    Malformed,
}

/// A compiled filter. Disabled checks are dropped at compile time so the hot
/// loop only does configured work.
#[derive(Debug, Clone)]
pub struct FilterEngine {
    filters: Filters,
    // Precomputed "is this tier needed at all?" flags.
    needs_prescan_component: bool,
    needs_prescan_isotope: bool,
    needs_parser: bool,
}

impl FilterEngine {
    /// Compiles a [`Filters`] into an engine.
    pub fn compile(filters: &Filters) -> Self {
        let require_single = filters.require_single_component_or_default();
        let forbid_isotopes = !filters.allow_isotopes_or_default();
        // The parser tier is needed if any of the parser-backed checks are
        // configured: atom bounds, composition, radicals, charge, or mass.
        let needs_parser = filters.min_atoms.is_some()
            || filters.max_atoms.is_some()
            || filters.elements.is_some()
            || !filters.allow_radicals_or_default()
            || filters.max_abs_charge.is_some()
            || filters.min_mass_da.is_some()
            || filters.max_mass_da.is_some();
        Self {
            needs_prescan_component: require_single,
            needs_prescan_isotope: forbid_isotopes,
            needs_parser,
            filters: filters.clone(),
        }
    }

    /// Evaluates a molecule.
    ///
    /// `shipped_mass` is an optional mass value supplied by a source's own
    /// column (e.g. ZINC's `mwt`, an average mass). When present and the
    /// configured [`MassKind`] is [`MassKind::Average`], the mass bounds are
    /// applied from it without parsing.
    pub fn check(&self, smiles: &[u8], shipped_mass: Option<f64>) -> FilterOutcome {
        // Tier 1: byte scans.
        if self.needs_prescan_component && prescan::is_multi_component(smiles) {
            return FilterOutcome::Reject(RejectReason::MultipleComponents);
        }
        if self.needs_prescan_isotope && prescan::has_isotope(smiles) {
            return FilterOutcome::Reject(RejectReason::Isotope);
        }

        // Tier 2: shipped-column mass fast path (only valid for Average mass).
        let mass_handled_by_column = self.try_shipped_mass(shipped_mass);
        if let Some(false) = mass_handled_by_column {
            return FilterOutcome::Reject(RejectReason::Mass);
        }

        if !self.needs_parser {
            return FilterOutcome::Pass;
        }

        // Tier 3: parse once, then run parser-backed checks.
        let Ok(text) = std::str::from_utf8(smiles) else {
            return FilterOutcome::Malformed;
        };
        let parsed: std::result::Result<Smiles, _> = text.parse();
        let Ok(s) = parsed else {
            return FilterOutcome::Malformed;
        };

        // Atom count.
        if (self.filters.min_atoms.is_some() || self.filters.max_atoms.is_some())
            && !parsed::atom_count_in_bounds(
                &s,
                self.filters.atom_count_mode_or_default(),
                self.filters.min_atoms,
                self.filters.max_atoms,
            )
        {
            return FilterOutcome::Reject(RejectReason::AtomCount);
        }

        // Composition.
        if let Some(set) = &self.filters.elements
            && !parsed::composition_ok(&s, set)
        {
            return FilterOutcome::Reject(RejectReason::Composition);
        }

        // Radicals (parser tier is definitive).
        if !self.filters.allow_radicals_or_default() && parsed::has_radical(&s) {
            return FilterOutcome::Reject(RejectReason::Radical);
        }

        // Net charge.
        if let Some(max_q) = self.filters.max_abs_charge
            && parsed::net_charge(&s).unsigned_abs() > max_q
        {
            return FilterOutcome::Reject(RejectReason::Charge);
        }

        // Mass (only if not already satisfied by the shipped column).
        if mass_handled_by_column.is_none()
            && (self.filters.min_mass_da.is_some() || self.filters.max_mass_da.is_some())
        {
            let mass = parsed::mass_da(&s, self.filters.mass_kind_or_default());
            if !self.mass_in_bounds(mass) {
                return FilterOutcome::Reject(RejectReason::Mass);
            }
        }

        FilterOutcome::Pass
    }

    /// Applies the mass bounds from a shipped column value, if usable.
    ///
    /// Returns `Some(true)` if the column satisfied the bounds, `Some(false)`
    /// if it violated them, or `None` if the column can't be used (no bounds
    /// configured, no value supplied, or the configured mass kind is not
    /// `Average`, since shipped masses are average masses).
    fn try_shipped_mass(&self, shipped_mass: Option<f64>) -> Option<bool> {
        if self.filters.min_mass_da.is_none() && self.filters.max_mass_da.is_none() {
            return None;
        }
        if self.filters.mass_kind_or_default() != MassKind::Average {
            return None;
        }
        let mass = shipped_mass?;
        Some(self.mass_in_bounds(mass))
    }

    fn mass_in_bounds(&self, mass: f64) -> bool {
        if let Some(lo) = self.filters.min_mass_da
            && mass < lo
        {
            return false;
        }
        if let Some(hi) = self.filters.max_mass_da
            && mass > hi
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use elements_rs::Element;

    use super::*;
    use crate::config::filters::{AtomCount, ElementSet};

    #[test]
    fn empty_filters_pass_everything_parseable() {
        let engine = FilterEngine::compile(&Filters::builder().build().unwrap());
        // require_single_component defaults to true, so a salt is rejected.
        assert_eq!(
            engine.check(b"CC.O", None),
            FilterOutcome::Reject(RejectReason::MultipleComponents)
        );
        // isotopes forbidden by default.
        assert_eq!(
            engine.check(b"[13C]CO", None),
            FilterOutcome::Reject(RejectReason::Isotope)
        );
        // a plain neutral molecule passes (no parser tier needed for defaults).
        assert_eq!(engine.check(b"CCO", None), FilterOutcome::Pass);
    }

    #[test]
    fn allows_multi_component_when_configured() {
        let engine = FilterEngine::compile(
            &Filters::builder()
                .require_single_component(false)
                .build()
                .unwrap(),
        );
        assert_eq!(engine.check(b"CC.O", None), FilterOutcome::Pass);
    }

    #[test]
    fn composition_filter() {
        let f = Filters::builder()
            .elements(ElementSet::Whitelist(vec![
                Element::C,
                Element::H,
                Element::O,
            ]))
            .build()
            .unwrap();
        let engine = FilterEngine::compile(&f);
        assert_eq!(engine.check(b"CCO", None), FilterOutcome::Pass);
        assert_eq!(
            engine.check(b"CCS", None),
            FilterOutcome::Reject(RejectReason::Composition)
        );
    }

    #[test]
    fn atom_count_filter() {
        let f = Filters::builder()
            .min_atoms(2)
            .max_atoms(3)
            .atom_count_mode(AtomCount::Heavy)
            .build()
            .unwrap();
        let engine = FilterEngine::compile(&f);
        assert_eq!(
            engine.check(b"C", None),
            FilterOutcome::Reject(RejectReason::AtomCount)
        ); // 1 heavy
        assert_eq!(engine.check(b"CCO", None), FilterOutcome::Pass); // 3 heavy
        assert_eq!(
            engine.check(b"CCCCO", None),
            FilterOutcome::Reject(RejectReason::AtomCount)
        ); // 5 heavy
    }

    #[test]
    fn charge_filter() {
        let f = Filters::builder().max_abs_charge(0).build().unwrap();
        let engine = FilterEngine::compile(&f);
        assert_eq!(engine.check(b"CCO", None), FilterOutcome::Pass);
        assert_eq!(
            engine.check(b"CC(=O)[O-]", None),
            FilterOutcome::Reject(RejectReason::Charge)
        );
    }

    #[test]
    fn mass_filter_parser_path() {
        // Monoisotopic mass bound; benzene ~78.05, hexane ~86.1.
        let f = Filters::builder().max_mass_da(80.0).build().unwrap();
        let engine = FilterEngine::compile(&f);
        assert_eq!(engine.check(b"c1ccccc1", None), FilterOutcome::Pass);
        assert_eq!(
            engine.check(b"CCCCCC", None),
            FilterOutcome::Reject(RejectReason::Mass)
        );
    }

    #[test]
    fn mass_filter_shipped_column_average() {
        use crate::config::filters::MassKind;
        let f = Filters::builder()
            .max_mass_da(100.0)
            .mass_kind(MassKind::Average)
            .build()
            .unwrap();
        let engine = FilterEngine::compile(&f);
        // Shipped mass under the cap passes without needing to parse.
        assert_eq!(engine.check(b"CCO", Some(46.07)), FilterOutcome::Pass);
        // Shipped mass over the cap is rejected.
        assert_eq!(
            engine.check(b"CCO", Some(150.0)),
            FilterOutcome::Reject(RejectReason::Mass)
        );
        // No shipped value falls back to the parser path.
        assert_eq!(engine.check(b"CCO", None), FilterOutcome::Pass);
    }

    #[test]
    fn radical_filter() {
        let engine = FilterEngine::compile(&Filters::builder().build().unwrap());
        assert_eq!(
            engine.check(b"[CH3]", None),
            FilterOutcome::Reject(RejectReason::Radical)
        );
        assert_eq!(engine.check(b"CCO", None), FilterOutcome::Pass);
    }

    #[test]
    fn malformed_smiles() {
        // Needs the parser tier to be active to detect malformed input.
        let f = Filters::builder().max_abs_charge(0).build().unwrap();
        let engine = FilterEngine::compile(&f);
        assert_eq!(engine.check(b"C(C(C", None), FilterOutcome::Malformed);
    }
}
